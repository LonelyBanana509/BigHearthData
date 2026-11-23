//! SCRAM-SHA-256 as pure code (RFC 5802, RFC 7677): message parsing, proofs, and verifier
//! derivation. Shared by the gateway, the clients and `bhdata-control`. Lane P1.
//!
//! No I/O and no clock. The only outside input is randomness, and every function that needs a nonce
//! or a salt takes it as an argument, so tests (and the RFC 7677 vectors) are exact. The helpers
//! [`random_nonce`], [`Verifier::generate`] and [`ServerSecret::generate`] draw from the operating
//! system for production code.
//!
//! What it implements, and what the wire-protocol spec (section 2, decisions 6 and 7) fixes:
//!
//! * `SCRAM-SHA-256` without channel binding. A client that asks for it (`p=...`) is refused.
//! * Passwords go through SASLprep when they are valid UTF-8 and are used as given otherwise (and
//!   when SASLprep refuses them), as PostgreSQL does.
//! * A login that does not exist looks like one that does: [`Lookup::Unknown`] derives a decoy
//!   verifier from a server secret and the name, runs the same code, and fails at the last step with
//!   the same [`ScramError::ProofRejected`] as a wrong password.
//! * Every parser is strict: a message with a field it does not know is refused, and no message is
//!   longer than [`MAX_MESSAGE_LEN`].
//!
//! The flow, one value per step, each consumed by the next:
//!
//! ```text
//! client                                         server
//! Client::start        --client_first-->         ClientFirst::parse, look the login up,
//!                                                ServerExchange::begin
//! AwaitServerFirst::respond <--server_first--
//!                      --client_final-->         ServerExchange::finish
//! AwaitServerFinal::verify  <--server_final--
//! ```
//!
//! `ScramVerifier` in `bhdata-api` is the stored form of a login. `bhdata-proto` cannot depend on
//! `bhdata-api` (`deps.allow`), so this module has its own [`Verifier`] with the same four fields;
//! the caller converts, field by field.

use std::borrow::Cow;
use std::fmt;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The mechanism name, as sent in the `auth` message.
pub const MECHANISM: &str = "SCRAM-SHA-256";

/// Longest SCRAM message either side accepts. It equals the longest string before login
/// (`wire-protocol.md`, section 2, decision 4), so a parser never needs more than that.
pub const MAX_MESSAGE_LEN: usize = 4096;

/// Longest nonce either side accepts.
pub const MAX_NONCE_LEN: usize = 256;

/// Longest salt a client accepts from a server (and a verifier may hold).
pub const MAX_SALT_LEN: usize = 64;

/// Most PBKDF2 iterations a client accepts from a server: a hostile server cannot make a client
/// compute for hours. A verifier may not hold more either.
pub const MAX_ITERATIONS: u32 = 10_000_000;

/// The iteration count for new verifiers (a placeholder until M11, as in the spec).
pub const DEFAULT_ITERATIONS: u32 = 100_000;

/// Salt length of a verifier made by [`Verifier::generate`] and of a decoy.
pub const SALT_LEN: usize = 16;

/// Random bytes in a nonce made by [`random_nonce`] (it is 32 characters of base64).
const NONCE_BYTES: usize = 24;

/// Base64 of the GS2 header `n,,` and of `y,,`: the `c=` value of the last client message.
const CBIND_N: &str = "biws";
const CBIND_Y: &str = "eSws";

/// Why a SCRAM step failed. The gateway answers every one of these the same way to the client
/// (`unauthorized` or an `error` frame, see the spec); the variants are for the log and the tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScramError {
    /// The message does not follow the grammar, or is too long.
    #[error("malformed SCRAM message: {0}")]
    Malformed(&'static str),
    /// A valid SCRAM feature this implementation does not offer (channel binding, authzid,
    /// extensions).
    #[error("unsupported SCRAM feature: {0}")]
    Unsupported(&'static str),
    /// The nonce is not the one that was sent.
    #[error("SCRAM nonce mismatch")]
    NonceMismatch,
    /// The iteration count is zero or above [`MAX_ITERATIONS`].
    #[error("SCRAM iteration count out of range")]
    IterationsOutOfRange,
    /// The client's proof is wrong. This is also the answer for a login that does not exist.
    #[error("SCRAM proof rejected")]
    ProofRejected,
    /// The server's signature is wrong: the server does not know the password verifier.
    #[error("SCRAM server signature does not match")]
    ServerSignatureMismatch,
    /// The server sent `e=...` instead of a signature.
    #[error("the server refused the login")]
    ServerRejected,
    /// The operating system gave no randomness.
    #[error("no randomness available")]
    Entropy,
}

type Result<T> = std::result::Result<T, ScramError>;

// ---------------------------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------------------------

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC accepts a key of any length, so `new_from_slice` has no failing case.
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Compares two keys without stopping at the first difference.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn xor(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (o, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *o = x ^ y;
    }
    out
}

/// SASLprep (RFC 4013) when the password is valid UTF-8 and SASLprep accepts it; the bytes as
/// given otherwise.
fn prepare_password(password: &[u8]) -> Cow<'_, [u8]> {
    match std::str::from_utf8(password) {
        Ok(text) => match stringprep::saslprep(text) {
            Ok(Cow::Borrowed(_)) | Err(_) => Cow::Borrowed(password),
            Ok(Cow::Owned(prepared)) => Cow::Owned(prepared.into_bytes()),
        },
        Err(_) => Cow::Borrowed(password),
    }
}

fn salted_password(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let prepared = prepare_password(password);
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&prepared, salt, iterations, &mut out);
    out
}

fn check_iterations(iterations: u32) -> Result<()> {
    if (1..=MAX_ITERATIONS).contains(&iterations) {
        Ok(())
    } else {
        Err(ScramError::IterationsOutOfRange)
    }
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::fill(buf).map_err(|_| ScramError::Entropy)
}

/// A fresh nonce: 32 printable characters from 24 random bytes. Use it for the client nonce and
/// for the server nonce.
///
/// # Errors
/// [`ScramError::Entropy`] if the operating system gives no randomness.
pub fn random_nonce() -> Result<String> {
    let mut bytes = [0u8; NONCE_BYTES];
    fill_random(&mut bytes)?;
    Ok(STANDARD.encode(bytes))
}

// ---------------------------------------------------------------------------------------------
// Verifier and the decoy for unknown names
// ---------------------------------------------------------------------------------------------

/// What the server stores for a login (RFC 5802): never the password.
///
/// Same four fields as `bhdata_api::ScramVerifier`; see the module note for why there are two.
#[derive(Clone, PartialEq, Eq)]
pub struct Verifier {
    /// The salt.
    pub salt: Vec<u8>,
    /// The PBKDF2 iteration count.
    pub iterations: u32,
    /// `H(HMAC(SaltedPassword, "Client Key"))`.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`.
    pub server_key: [u8; 32],
}

impl fmt::Debug for Verifier {
    // The keys and the salt never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Derives the verifier of `password` for a given salt and iteration count. The password is
    /// SASLprep-ed when it is valid UTF-8 (see the module note).
    ///
    /// # Errors
    /// [`ScramError::IterationsOutOfRange`] for a count of 0 or above [`MAX_ITERATIONS`];
    /// [`ScramError::Malformed`] for an empty salt or one longer than [`MAX_SALT_LEN`].
    pub fn derive(password: &[u8], salt: Vec<u8>, iterations: u32) -> Result<Self> {
        check_iterations(iterations)?;
        if salt.is_empty() || salt.len() > MAX_SALT_LEN {
            return Err(ScramError::Malformed("salt length"));
        }
        let salted = salted_password(password, &salt, iterations);
        let client_key = hmac(&salted, b"Client Key");
        Ok(Self {
            salt,
            iterations,
            stored_key: sha256(&client_key),
            server_key: hmac(&salted, b"Server Key"),
        })
    }

    /// Derives a verifier with a random salt of [`SALT_LEN`] bytes. This is what
    /// `bhdatactl login create` calls.
    ///
    /// # Errors
    /// As [`Verifier::derive`], and [`ScramError::Entropy`].
    pub fn generate(password: &[u8], iterations: u32) -> Result<Self> {
        let mut salt = vec![0u8; SALT_LEN];
        fill_random(&mut salt)?;
        Self::derive(password, salt, iterations)
    }
}

/// The server secret behind the answer for a login that does not exist (spec, decision 7). One
/// per gateway installation, stable across restarts, or an attacker could tell a name is unknown
/// because its salt changed between two attempts.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerSecret([u8; 32]);

impl fmt::Debug for ServerSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServerSecret(..)")
    }
}

impl ServerSecret {
    /// Wraps 32 bytes the operator keeps (the control plane stores them).
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// A new random secret.
    ///
    /// # Errors
    /// [`ScramError::Entropy`].
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 32];
        fill_random(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// The bytes, for the control plane to store.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The verifier that stands in for a login called `name`: the same name and secret always give
    /// the same salt, and no password fits its keys.
    #[must_use]
    pub fn decoy(&self, name: &str, iterations: u32) -> Verifier {
        let part = |label: &[u8]| {
            let mut data = Vec::with_capacity(label.len() + 1 + name.len());
            data.extend_from_slice(label);
            data.push(0);
            data.extend_from_slice(name.as_bytes());
            hmac(&self.0, &data)
        };
        Verifier {
            salt: part(b"salt")[..SALT_LEN].to_vec(),
            iterations,
            stored_key: part(b"stored key"),
            server_key: part(b"server key"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------------------------

fn check_len(message: &str) -> Result<()> {
    if message.len() > MAX_MESSAGE_LEN {
        Err(ScramError::Malformed("message too long"))
    } else {
        Ok(())
    }
}

/// A nonce is printable ASCII without a comma, and not too long (RFC 5802, `printable`).
fn check_nonce(nonce: &str) -> Result<()> {
    let ok = !nonce.is_empty()
        && nonce.len() <= MAX_NONCE_LEN
        && nonce
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b',');
    if ok {
        Ok(())
    } else {
        Err(ScramError::Malformed("nonce"))
    }
}

/// Reads `<key>=<value>` from the front of `rest`; the value ends at the next comma or at the end.
/// `rest` moves past the comma.
fn take<'a>(rest: &mut &'a str, key: char) -> Result<&'a str> {
    let mut chars = rest.chars();
    if chars.next() != Some(key) || chars.next() != Some('=') {
        return Err(ScramError::Malformed("unexpected or missing attribute"));
    }
    let body = &rest[2..];
    match body.split_once(',') {
        Some((value, tail)) => {
            *rest = tail;
            Ok(value)
        }
        None => {
            *rest = "";
            Ok(body)
        }
    }
}

fn expect_end(rest: &str) -> Result<()> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(ScramError::Malformed("extra attribute"))
    }
}

fn decode_b64(value: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(value)
        .map_err(|_| ScramError::Malformed("base64"))
}

fn decode_key(value: &str) -> Result<[u8; 32]> {
    decode_b64(value)?
        .try_into()
        .map_err(|_| ScramError::Malformed("proof or signature length"))
}

fn escape_name(name: &str) -> String {
    name.replace('=', "=3D").replace(',', "=2C")
}

fn unescape_name(escaped: &str) -> Result<String> {
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped;
    while let Some(at) = rest.find('=') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        if let Some(after) = tail.strip_prefix("2C") {
            out.push(',');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("3D") {
            out.push('=');
            rest = after;
        } else {
            return Err(ScramError::Malformed("user name escape"));
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Parses `i=<digits>`: ASCII digits only, so no sign and no space.
fn parse_iterations(value: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ScramError::Malformed("iteration count"));
    }
    let n: u32 = value
        .parse()
        .map_err(|_| ScramError::IterationsOutOfRange)?;
    check_iterations(n)?;
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------------------------

/// The GS2 header the client announced. Only the two forms without channel binding exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gs2 {
    /// `n,,`: the client does not support channel binding.
    N,
    /// `y,,`: the client supports it but thinks the server does not.
    Y,
}

impl Gs2 {
    fn cbind_input(self) -> &'static str {
        match self {
            Gs2::N => CBIND_N,
            Gs2::Y => CBIND_Y,
        }
    }
}

/// The first client message, parsed: `n,,n=<name>,r=<nonce>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFirst {
    user: String,
    nonce: String,
    gs2: Gs2,
    /// The text after the GS2 header, as sent (it goes into the auth message).
    bare: String,
}

impl ClientFirst {
    /// Parses the first client message.
    ///
    /// # Errors
    /// [`ScramError::Malformed`] for anything outside the grammar or longer than
    /// [`MAX_MESSAGE_LEN`]; [`ScramError::Unsupported`] for channel binding (`p=`), an authzid
    /// (`a=`) or an extension.
    pub fn parse(message: &str) -> Result<Self> {
        check_len(message)?;
        let (flag, rest) = message
            .split_once(',')
            .ok_or(ScramError::Malformed("gs2 header"))?;
        let gs2 = match flag {
            "n" => Gs2::N,
            "y" => Gs2::Y,
            f if f.starts_with("p=") => {
                return Err(ScramError::Unsupported("channel binding"));
            }
            _ => return Err(ScramError::Malformed("gs2 header")),
        };
        let (authzid, bare) = rest
            .split_once(',')
            .ok_or(ScramError::Malformed("gs2 header"))?;
        if !authzid.is_empty() {
            return Err(ScramError::Unsupported("authzid"));
        }
        if bare.starts_with("m=") {
            return Err(ScramError::Unsupported("mandatory extension"));
        }
        let mut cursor = bare;
        let name = take(&mut cursor, 'n')?;
        let nonce = take(&mut cursor, 'r')?;
        expect_end(cursor)?;
        if name.is_empty() {
            return Err(ScramError::Malformed("empty user name"));
        }
        check_nonce(nonce)?;
        Ok(Self {
            user: unescape_name(name)?,
            nonce: nonce.to_owned(),
            gs2,
            bare: bare.to_owned(),
        })
    }

    /// The name the client wants to log in as, as sent (it may not be a valid login id).
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }
}

/// The last client message, parsed: `c=<base64>,r=<nonce>,p=<base64 proof>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFinal {
    channel_binding: String,
    nonce: String,
    proof: [u8; 32],
    /// The message without `,p=...`, as sent (it goes into the auth message).
    without_proof: String,
}

impl ClientFinal {
    /// Parses the last client message.
    ///
    /// # Errors
    /// [`ScramError::Malformed`] for anything outside the grammar, a proof that is not 32 bytes of
    /// base64, or a message longer than [`MAX_MESSAGE_LEN`].
    pub fn parse(message: &str) -> Result<Self> {
        check_len(message)?;
        let mut cursor = message;
        let channel_binding = take(&mut cursor, 'c')?;
        let nonce = take(&mut cursor, 'r')?;
        let proof_text = take(&mut cursor, 'p')?;
        expect_end(cursor)?;
        check_nonce(nonce)?;
        let proof = decode_key(proof_text)?;
        // `take` consumed `,` between the attributes, so what precedes `,p=` is everything but the
        // last attribute and its separator.
        let without_proof = message
            .len()
            .checked_sub(proof_text.len() + 3)
            .and_then(|end| message.get(..end))
            .ok_or(ScramError::Malformed("final message"))?;
        Ok(Self {
            channel_binding: channel_binding.to_owned(),
            nonce: nonce.to_owned(),
            proof,
            without_proof: without_proof.to_owned(),
        })
    }
}

/// Which verifier the server uses for the name in the first message.
#[derive(Debug, Clone, Copy)]
pub enum Lookup<'a> {
    /// The login exists (and is not disabled; a disabled login is `Unknown` here, so it fails like
    /// a wrong password).
    Known(&'a Verifier),
    /// There is no such login: answer from a decoy.
    Unknown {
        /// The installation's secret.
        secret: &'a ServerSecret,
        /// The iteration count a real verifier has (use the configured default), so that the count
        /// does not tell the two cases apart.
        iterations: u32,
    },
}

/// The server's state between its first and last message. One value per login attempt; `finish`
/// consumes it, so an exchange cannot be answered twice.
pub struct ServerExchange {
    stored_key: [u8; 32],
    server_key: [u8; 32],
    nonce: String,
    /// `client-first-bare "," server-first`; the last client message is appended to it.
    auth_prefix: String,
    cbind_input: &'static str,
}

impl fmt::Debug for ServerExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServerExchange(..)")
    }
}

impl ServerExchange {
    /// Answers the first client message. Returns the exchange and the `server_first` message to
    /// send. `server_nonce` is fresh randomness (see [`random_nonce`]); the nonce on the wire is
    /// the client's followed by it.
    ///
    /// # Errors
    /// [`ScramError::Malformed`] for a `server_nonce` that is not printable text without commas, and
    /// the errors of [`Verifier::derive`] for a verifier that is out of range.
    pub fn begin(
        first: &ClientFirst,
        lookup: Lookup<'_>,
        server_nonce: &str,
    ) -> Result<(Self, String)> {
        check_nonce(server_nonce)?;
        let verifier: Cow<'_, Verifier> = match lookup {
            Lookup::Known(v) => Cow::Borrowed(v),
            Lookup::Unknown { secret, iterations } => {
                Cow::Owned(secret.decoy(&first.user, iterations))
            }
        };
        check_iterations(verifier.iterations)?;
        let nonce = format!("{}{}", first.nonce, server_nonce);
        let server_first = format!(
            "r={nonce},s={},i={}",
            STANDARD.encode(&verifier.salt),
            verifier.iterations
        );
        let auth_prefix = format!("{},{}", first.bare, server_first);
        let exchange = Self {
            stored_key: verifier.stored_key,
            server_key: verifier.server_key,
            nonce,
            auth_prefix,
            cbind_input: first.gs2.cbind_input(),
        };
        Ok((exchange, server_first))
    }

    /// Checks the last client message. On success returns the `server_final` message to send.
    ///
    /// A wrong password and a login that does not exist both end in
    /// [`ScramError::ProofRejected`]; the two cannot be told apart from here or from the wire.
    ///
    /// # Errors
    /// [`ScramError::Malformed`] and [`ScramError::Unsupported`] for a bad message (also a changed
    /// channel-binding field), [`ScramError::NonceMismatch`], [`ScramError::ProofRejected`].
    pub fn finish(self, client_final: &str) -> Result<String> {
        let parsed = ClientFinal::parse(client_final)?;
        if parsed.channel_binding != self.cbind_input {
            return Err(ScramError::Malformed("channel binding field"));
        }
        if parsed.nonce != self.nonce {
            return Err(ScramError::NonceMismatch);
        }
        let auth_message = format!("{},{}", self.auth_prefix, parsed.without_proof);
        let client_signature = hmac(&self.stored_key, auth_message.as_bytes());
        let client_key = xor(&parsed.proof, &client_signature);
        if !ct_eq(&sha256(&client_key), &self.stored_key) {
            return Err(ScramError::ProofRejected);
        }
        let server_signature = hmac(&self.server_key, auth_message.as_bytes());
        Ok(format!("v={}", STANDARD.encode(server_signature)))
    }
}

// ---------------------------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------------------------

/// Starts a login. Use [`Client::start`].
pub struct Client;

impl Client {
    /// Begins a login for `user` with `password` (bytes: SASLprep-ed when valid UTF-8). `nonce` is
    /// fresh randomness (see [`random_nonce`]). Returns the state and the `client_first` message to
    /// send.
    ///
    /// # Errors
    /// [`ScramError::Malformed`] for an empty `user` or a `nonce` that is not printable text without
    /// commas.
    pub fn start(user: &str, password: &[u8], nonce: String) -> Result<(AwaitServerFirst, String)> {
        if user.is_empty() {
            return Err(ScramError::Malformed("empty user name"));
        }
        check_nonce(&nonce)?;
        let bare = format!("n={},r={}", escape_name(user), nonce);
        let message = format!("n,,{bare}");
        check_len(&message)?;
        let state = AwaitServerFirst {
            password: password.to_vec(),
            nonce,
            bare,
        };
        Ok((state, message))
    }
}

/// The client after `client_first`, waiting for `server_first`.
pub struct AwaitServerFirst {
    password: Vec<u8>,
    nonce: String,
    bare: String,
}

impl fmt::Debug for AwaitServerFirst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AwaitServerFirst(..)")
    }
}

impl AwaitServerFirst {
    /// Takes the `server_first` message, does the PBKDF2 work, and returns the next state and the
    /// `client_final` message to send.
    ///
    /// # Errors
    /// [`ScramError::Malformed`], [`ScramError::Unsupported`] for an extension,
    /// [`ScramError::NonceMismatch`] if the server's nonce does not start with the client's (or adds
    /// nothing), and [`ScramError::IterationsOutOfRange`].
    pub fn respond(self, server_first: &str) -> Result<(AwaitServerFinal, String)> {
        check_len(server_first)?;
        if server_first.starts_with("m=") {
            return Err(ScramError::Unsupported("mandatory extension"));
        }
        let mut cursor = server_first;
        let nonce = take(&mut cursor, 'r')?;
        let salt_text = take(&mut cursor, 's')?;
        let iterations_text = take(&mut cursor, 'i')?;
        expect_end(cursor)?;
        check_nonce(nonce)?;
        if nonce.len() <= self.nonce.len() || !nonce.starts_with(&self.nonce) {
            return Err(ScramError::NonceMismatch);
        }
        let salt = decode_b64(salt_text)?;
        if salt.is_empty() || salt.len() > MAX_SALT_LEN {
            return Err(ScramError::Malformed("salt length"));
        }
        let iterations = parse_iterations(iterations_text)?;

        let salted = salted_password(&self.password, &salt, iterations);
        let client_key = hmac(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac(&salted, b"Server Key");

        let without_proof = format!("c={CBIND_N},r={nonce}");
        let auth_message = format!("{},{},{}", self.bare, server_first, without_proof);
        let client_signature = hmac(&stored_key, auth_message.as_bytes());
        let proof = xor(&client_key, &client_signature);
        let message = format!("{without_proof},p={}", STANDARD.encode(proof));
        let expected_server_signature = hmac(&server_key, auth_message.as_bytes());
        Ok((
            AwaitServerFinal {
                expected_server_signature,
            },
            message,
        ))
    }
}

/// The client after `client_final`, waiting for `server_final`.
pub struct AwaitServerFinal {
    expected_server_signature: [u8; 32],
}

impl fmt::Debug for AwaitServerFinal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AwaitServerFinal(..)")
    }
}

impl AwaitServerFinal {
    /// Checks the server's signature: the server proves it knows the verifier too.
    ///
    /// # Errors
    /// [`ScramError::ServerRejected`] for `e=...`, [`ScramError::Malformed`] for anything outside the
    /// grammar, [`ScramError::ServerSignatureMismatch`] for a wrong signature.
    pub fn verify(self, server_final: &str) -> Result<()> {
        check_len(server_final)?;
        if server_final.starts_with("e=") {
            return Err(ScramError::ServerRejected);
        }
        let mut cursor = server_final;
        let signature = take(&mut cursor, 'v')?;
        expect_end(cursor)?;
        let signature = decode_key(signature)?;
        if ct_eq(&signature, &self.expected_server_signature) {
            Ok(())
        } else {
            Err(ScramError::ServerSignatureMismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // RFC 7677, section 3.
    const RFC_USER: &str = "user";
    const RFC_PASSWORD: &[u8] = b"pencil";
    const RFC_CLIENT_NONCE: &str = "rOprNGfwEbeRWgbNEkqO";
    const RFC_SERVER_NONCE: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    const RFC_SALT: &str = "W22ZaJ0SNY7soEsUEjb6gQ==";
    const RFC_CLIENT_FIRST: &str = "n,,n=user,r=rOprNGfwEbeRWgbNEkqO";
    const RFC_SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    const RFC_CLIENT_FINAL: &str = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
    const RFC_SERVER_FINAL: &str = "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";

    fn rfc_verifier() -> Verifier {
        let salt = STANDARD.decode(RFC_SALT).unwrap();
        Verifier::derive(RFC_PASSWORD, salt, 4096).unwrap()
    }

    /// Runs both sides with the given verifier lookup; returns the server's result.
    fn exchange(
        user: &str,
        password: &[u8],
        lookup: Lookup<'_>,
        client_nonce: &str,
        server_nonce: &str,
    ) -> Result<()> {
        let (client, first) = Client::start(user, password, client_nonce.to_owned()).unwrap();
        let parsed = ClientFirst::parse(&first).unwrap();
        let (server, server_first) = ServerExchange::begin(&parsed, lookup, server_nonce).unwrap();
        let (client, client_final) = client.respond(&server_first).unwrap();
        let server_final = server.finish(&client_final)?;
        client.verify(&server_final).unwrap();
        Ok(())
    }

    // ----- W2: the RFC 7677 vectors -----

    #[test]
    fn rfc_7677_client_side_matches_every_message() {
        let (client, first) =
            Client::start(RFC_USER, RFC_PASSWORD, RFC_CLIENT_NONCE.to_owned()).unwrap();
        assert_eq!(first, RFC_CLIENT_FIRST);
        let (client, client_final) = client.respond(RFC_SERVER_FIRST).unwrap();
        assert_eq!(client_final, RFC_CLIENT_FINAL);
        client.verify(RFC_SERVER_FINAL).unwrap();
    }

    #[test]
    fn rfc_7677_server_side_matches_every_message() {
        let verifier = rfc_verifier();
        let first = ClientFirst::parse(RFC_CLIENT_FIRST).unwrap();
        assert_eq!(first.user(), RFC_USER);
        let (server, server_first) =
            ServerExchange::begin(&first, Lookup::Known(&verifier), RFC_SERVER_NONCE).unwrap();
        assert_eq!(server_first, RFC_SERVER_FIRST);
        assert_eq!(server.finish(RFC_CLIENT_FINAL).unwrap(), RFC_SERVER_FINAL);
    }

    #[test]
    fn rfc_7677_client_refuses_a_wrong_server_signature() {
        let (client, _) =
            Client::start(RFC_USER, RFC_PASSWORD, RFC_CLIENT_NONCE.to_owned()).unwrap();
        let (client, _) = client.respond(RFC_SERVER_FIRST).unwrap();
        let forged = "v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert_eq!(
            client.verify(forged),
            Err(ScramError::ServerSignatureMismatch)
        );
    }

    // ----- W2: a wrong password and an unknown login are the same failure -----

    #[test]
    fn a_wrong_password_is_proof_rejected() {
        let v = Verifier::derive(b"right", vec![1; 16], 4).unwrap();
        let result = exchange("ann", b"wrong", Lookup::Known(&v), "cn", "sn");
        assert_eq!(result, Err(ScramError::ProofRejected));
    }

    #[test]
    fn an_unknown_login_fails_with_the_same_error() {
        let secret = ServerSecret::new([5; 32]);
        let unknown = Lookup::Unknown {
            secret: &secret,
            iterations: 4,
        };
        let known = Verifier::derive(b"right", vec![1; 16], 4).unwrap();
        let wrong = exchange("ann", b"wrong", Lookup::Known(&known), "cn", "sn");
        let missing = exchange("nobody", b"anything", unknown, "cn", "sn");
        assert_eq!(wrong, missing);
        assert_eq!(missing, Err(ScramError::ProofRejected));
    }

    #[test]
    fn an_unknown_login_gets_the_same_looking_first_answer_every_time() {
        let secret = ServerSecret::new([5; 32]);
        let lookup = Lookup::Unknown {
            secret: &secret,
            iterations: DEFAULT_ITERATIONS,
        };
        let salt_of = |name: &str, client_nonce: &str| {
            let first = ClientFirst::parse(&format!("n,,n={name},r={client_nonce}")).unwrap();
            let (_, server_first) = ServerExchange::begin(&first, lookup, "sn").unwrap();
            // `s=...,i=...` without the nonce
            server_first.split_once(',').unwrap().1.to_owned()
        };
        // Same name: the same salt and count, whatever the nonces are.
        assert_eq!(salt_of("ghost", "one"), salt_of("ghost", "two"));
        // A different name or a different secret gives a different salt.
        assert_ne!(salt_of("ghost", "one"), salt_of("spirit", "one"));
        let other = ServerSecret::new([6; 32]);
        assert_ne!(
            secret.decoy("ghost", 1).salt,
            other.decoy("ghost", 1).salt,
            "the salt depends on the secret"
        );
        // The shape is a real one: SALT_LEN bytes and the count asked for.
        let decoy = secret.decoy("ghost", DEFAULT_ITERATIONS);
        assert_eq!(decoy.salt.len(), SALT_LEN);
        assert_eq!(decoy.iterations, DEFAULT_ITERATIONS);
    }

    #[test]
    fn the_right_password_works_and_can_be_checked_twice_with_fresh_nonces() {
        let v = Verifier::generate(b"s3cret", 4).unwrap();
        for _ in 0..3 {
            let cn = random_nonce().unwrap();
            let sn = random_nonce().unwrap();
            exchange("ann", b"s3cret", Lookup::Known(&v), &cn, &sn).unwrap();
        }
    }

    // ----- Replays, nonces, channel binding -----

    #[test]
    fn a_client_final_from_another_exchange_is_refused() {
        let v = Verifier::derive(b"pw", vec![2; 16], 4).unwrap();
        let begin = |cn: &str, sn: &str| {
            let (client, first) = Client::start("ann", b"pw", cn.to_owned()).unwrap();
            let parsed = ClientFirst::parse(&first).unwrap();
            let (server, server_first) =
                ServerExchange::begin(&parsed, Lookup::Known(&v), sn).unwrap();
            (client, server, server_first)
        };
        let (c1, _s1, sf1) = begin("nonce-a", "srv-1");
        let (_c2, s2, _sf2) = begin("nonce-b", "srv-2");
        let (_, final1) = c1.respond(&sf1).unwrap();
        assert_eq!(s2.finish(&final1), Err(ScramError::NonceMismatch));
    }

    #[test]
    fn a_changed_server_nonce_is_refused_by_the_client() {
        let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
        let err = client.respond("r=zzzdef,s=AQEBAQ==,i=4").unwrap_err();
        assert_eq!(err, ScramError::NonceMismatch);
        let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
        // The server added nothing to the client's nonce.
        let err = client.respond("r=abc,s=AQEBAQ==,i=4").unwrap_err();
        assert_eq!(err, ScramError::NonceMismatch);
    }

    #[test]
    fn channel_binding_is_refused_and_the_c_field_must_match_the_header() {
        assert_eq!(
            ClientFirst::parse("p=tls-exporter,,n=ann,r=abc"),
            Err(ScramError::Unsupported("channel binding"))
        );
        // A client that sent `y,,` must say so in `c=`; `n,,` and `y,,` are not interchangeable.
        let v = Verifier::derive(b"pw", vec![3; 16], 4).unwrap();
        let first = ClientFirst::parse("y,,n=ann,r=abc").unwrap();
        let (server, server_first) =
            ServerExchange::begin(&first, Lookup::Known(&v), "def").unwrap();
        let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
        let (_, client_final) = client.respond(&server_first).unwrap();
        // The client answered with `c=biws` (n,,) to a header that said `y,,`.
        assert!(matches!(
            server.finish(&client_final),
            Err(ScramError::Malformed("channel binding field"))
        ));
    }

    #[test]
    fn a_server_that_refuses_is_reported() {
        let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
        let (client, _) = client.respond("r=abcdef,s=AQEBAQ==,i=4").unwrap();
        assert_eq!(
            client.verify("e=invalid-proof"),
            Err(ScramError::ServerRejected)
        );
    }

    // ----- Parsers -----

    #[test]
    fn client_first_refuses_what_it_does_not_offer() {
        for (message, expected) in [
            ("n,a=other,n=ann,r=abc", ScramError::Unsupported("authzid")),
            (
                "n,,m=ext,n=ann,r=abc",
                ScramError::Unsupported("mandatory extension"),
            ),
            (
                "n,,n=ann,r=abc,x=1",
                ScramError::Malformed("extra attribute"),
            ),
            (
                "n,,r=abc,n=ann",
                ScramError::Malformed("unexpected or missing attribute"),
            ),
            ("n,,n=,r=abc", ScramError::Malformed("empty user name")),
            ("n,,n=ann,r=", ScramError::Malformed("nonce")),
            ("n,,n=ann,r=a,b", ScramError::Malformed("extra attribute")),
            ("n,,n=a=b,r=abc", ScramError::Malformed("user name escape")),
            ("n,,n=a=2,r=abc", ScramError::Malformed("user name escape")),
            ("x,,n=ann,r=abc", ScramError::Malformed("gs2 header")),
            ("n", ScramError::Malformed("gs2 header")),
            ("n,", ScramError::Malformed("gs2 header")),
            ("", ScramError::Malformed("gs2 header")),
        ] {
            assert_eq!(ClientFirst::parse(message), Err(expected), "{message:?}");
        }
    }

    #[test]
    fn nonces_are_printable_ascii_without_commas() {
        assert!(ClientFirst::parse("n,,n=ann,r=a b").is_err());
        assert!(ClientFirst::parse("n,,n=ann,r=caf\u{e9}").is_err());
        assert!(ClientFirst::parse("n,,n=ann,r=a\u{7}").is_err());
        let long = "a".repeat(MAX_NONCE_LEN + 1);
        assert!(ClientFirst::parse(&format!("n,,n=ann,r={long}")).is_err());
        let ok = "a".repeat(MAX_NONCE_LEN);
        assert!(ClientFirst::parse(&format!("n,,n=ann,r={ok}")).is_ok());
    }

    #[test]
    fn no_message_may_exceed_the_limit() {
        let long = "x".repeat(MAX_MESSAGE_LEN);
        assert!(ClientFirst::parse(&format!("n,,n={long},r=abc")).is_err());
        assert!(ClientFinal::parse(&format!("c=biws,r=abc,p={long}")).is_err());
        let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
        assert!(client.respond(&format!("r=abcd,s={long},i=4")).is_err());
        assert!(Client::start(&long, b"pw", "abc".to_owned()).is_err());
    }

    #[test]
    fn client_final_refuses_bad_proofs_and_extra_fields() {
        let good = RFC_CLIENT_FINAL;
        assert!(ClientFinal::parse(good).is_ok());
        let short = "c=biws,r=abc,p=AAAA";
        assert_eq!(
            ClientFinal::parse(short),
            Err(ScramError::Malformed("proof or signature length"))
        );
        assert!(ClientFinal::parse(&format!("{good},x=1")).is_err());
        assert!(ClientFinal::parse("c=biws,p=AAAA,r=abc").is_err());
        assert!(ClientFinal::parse("c=biws,r=abc,p=!!!!").is_err());
        // Padding must be right: no unpadded or non-canonical base64.
        let unpadded = good.trim_end_matches('=');
        assert!(ClientFinal::parse(unpadded).is_err());
    }

    #[test]
    fn server_first_refuses_bad_counts_and_salts() {
        let try_with = |server_first: &str| {
            let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
            client.respond(server_first).map(|_| ())
        };
        assert!(try_with("r=abcd,s=AQEBAQ==,i=4").is_ok());
        for bad in [
            "r=abcd,s=AQEBAQ==,i=0",
            "r=abcd,s=AQEBAQ==,i=+4",
            "r=abcd,s=AQEBAQ==,i=-4",
            "r=abcd,s=AQEBAQ==,i=4 ",
            "r=abcd,s=AQEBAQ==,i=",
            "r=abcd,s=AQEBAQ==,i=4294967295",
            "r=abcd,s=AQEBAQ==,i=99999999999999999999",
            "r=abcd,s=AQEBAQ==,i=10000001",
            "r=abcd,s=,i=4",
            "r=abcd,s=AQEBAQ=,i=4",
            "r=abcd,i=4,s=AQEBAQ==",
            "r=abcd,s=AQEBAQ==,i=4,x=1",
            "m=ext,r=abcd,s=AQEBAQ==,i=4",
        ] {
            assert!(try_with(bad).is_err(), "{bad:?} should be refused");
        }
        // The limit itself is accepted by the parser (not run: 10 million rounds is slow in debug).
        assert_eq!(parse_iterations("10000000"), Ok(MAX_ITERATIONS));
        let huge_salt = STANDARD.encode(vec![1u8; MAX_SALT_LEN + 1]);
        assert!(try_with(&format!("r=abcd,s={huge_salt},i=4")).is_err());
    }

    #[test]
    fn user_names_with_equals_and_commas_survive_the_wire() {
        // The login id alphabet has neither, but the grammar must still hold.
        for name in ["a=b", "a,b", "=,=,", "plain"] {
            let (_, first) = Client::start(name, b"pw", "abc".to_owned()).unwrap();
            assert_eq!(ClientFirst::parse(&first).unwrap().user(), name);
        }
        assert!(Client::start("", b"pw", "abc".to_owned()).is_err());
    }

    // ----- Verifier and passwords -----

    #[test]
    fn derive_checks_its_inputs() {
        assert_eq!(
            Verifier::derive(b"pw", vec![1; 16], 0),
            Err(ScramError::IterationsOutOfRange)
        );
        assert_eq!(
            Verifier::derive(b"pw", vec![1; 16], MAX_ITERATIONS + 1),
            Err(ScramError::IterationsOutOfRange)
        );
        assert!(Verifier::derive(b"pw", vec![], 4).is_err());
        assert!(Verifier::derive(b"pw", vec![1; MAX_SALT_LEN + 1], 4).is_err());
    }

    #[test]
    fn generate_makes_a_fresh_salt_each_time() {
        let a = Verifier::generate(b"pw", 4).unwrap();
        let b = Verifier::generate(b"pw", 4).unwrap();
        assert_eq!(a.salt.len(), SALT_LEN);
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.stored_key, b.stored_key);
    }

    #[test]
    fn passwords_are_saslprepped_when_they_can_be() {
        // RFC 4013 example: U+00AD (soft hyphen) maps to nothing, so "I\u{AD}X" is "IX".
        let salt = vec![9; 16];
        let a = Verifier::derive("I\u{ad}X".as_bytes(), salt.clone(), 4).unwrap();
        let b = Verifier::derive(b"IX", salt.clone(), 4).unwrap();
        assert_eq!(a, b);
        // Non-ASCII spaces become a plain space.
        let c = Verifier::derive("a\u{a0}b".as_bytes(), salt.clone(), 4).unwrap();
        let d = Verifier::derive(b"a b", salt.clone(), 4).unwrap();
        assert_eq!(c, d);
    }

    #[test]
    fn passwords_that_saslprep_refuses_or_that_are_not_text_are_used_as_given() {
        let salt = vec![9; 16];
        // Not UTF-8: used as given, no panic, and the login still works.
        let raw = [0xff, 0xfe, 0x00, 0x41];
        let v = Verifier::derive(&raw, salt.clone(), 4).unwrap();
        exchange("ann", &raw, Lookup::Known(&v), "cn", "sn").unwrap();
        // A control character is prohibited by SASLprep: used as given.
        let control = "pass\u{7}word".as_bytes();
        let v = Verifier::derive(control, salt.clone(), 4).unwrap();
        exchange("ann", control, Lookup::Known(&v), "cn", "sn").unwrap();
        // ... and it is not the same password as one without the control character.
        let plain = Verifier::derive(b"password", salt, 4).unwrap();
        assert_ne!(v.stored_key, plain.stored_key);
    }

    #[test]
    fn the_empty_password_is_a_password() {
        let v = Verifier::derive(b"", vec![1; 16], 4).unwrap();
        exchange("ann", b"", Lookup::Known(&v), "cn", "sn").unwrap();
        assert_eq!(
            exchange("ann", b"x", Lookup::Known(&v), "cn", "sn"),
            Err(ScramError::ProofRejected)
        );
    }

    #[test]
    fn secrets_never_reach_a_log_line() {
        let v = Verifier::derive(b"pw", vec![1, 2, 3, 4], 4096).unwrap();
        let text = format!("{v:?}");
        assert!(text.contains("4096"));
        assert!(!text.contains("stored_key") && !text.contains("server_key"));
        assert!(!text.contains("salt"));
        let secret = format!("{:?}", ServerSecret::new([7; 32]));
        assert!(!secret.contains('7'));
        let (state, _) = Client::start("ann", b"hunter2", "abc".to_owned()).unwrap();
        assert!(!format!("{state:?}").contains("hunter2"));
    }

    #[test]
    fn random_nonces_are_valid_and_different() {
        let a = random_nonce().unwrap();
        let b = random_nonce().unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        check_nonce(&a).unwrap();
        assert!(ServerSecret::generate().unwrap() != ServerSecret::generate().unwrap());
    }

    #[test]
    fn key_comparison_sees_every_byte() {
        let a = [3u8; 32];
        assert!(ct_eq(&a, &a));
        for i in 0..32 {
            let mut b = a;
            b[i] ^= 1;
            assert!(!ct_eq(&a, &b), "byte {i}");
        }
    }

    // ----- W16 (part): the parsers never panic -----

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn parsers_never_panic_on_any_text(text in ".{0,300}") {
            let _ = ClientFirst::parse(&text);
            let _ = ClientFinal::parse(&text);
            let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
            let _ = client.respond(&text);
            let (client, _) = Client::start("ann", b"pw", "abc".to_owned()).unwrap();
            if let Ok((state, _)) = client.respond("r=abcd,s=AQEBAQ==,i=1") {
                let _ = state.verify(&text);
            }
        }

        #[test]
        fn parsers_never_panic_on_grammar_shaped_text(
            name in "[a-zA-Z0-9=,]{0,12}",
            nonce in "[!-~]{0,20}",
            salt in "[A-Za-z0-9+/=]{0,24}",
            count in "[0-9+-]{0,12}",
            proof in "[A-Za-z0-9+/=]{0,48}",
        ) {
            let _ = ClientFirst::parse(&format!("n,,n={name},r={nonce}"));
            let _ = ClientFinal::parse(&format!("c=biws,r={nonce},p={proof}"));
            let (client, _) = Client::start("ann", b"pw", "ab".to_owned()).unwrap();
            let _ = client.respond(&format!("r=ab{nonce},s={salt},i={count}"));
        }

        #[test]
        fn any_login_and_password_round_trip(
            name in "[A-Za-z0-9._:-]{1,64}",
            password in proptest::collection::vec(any::<u8>(), 0..40),
            wrong in proptest::collection::vec(any::<u8>(), 0..40),
        ) {
            let v = Verifier::derive(&password, vec![4; SALT_LEN], 2).unwrap();
            prop_assert!(exchange(&name, &password, Lookup::Known(&v), "cn", "sn").is_ok());
            // A different password fails, unless SASLprep makes the two the same password.
            if prepare_password(&wrong) != prepare_password(&password) {
                prop_assert_eq!(
                    exchange(&name, &wrong, Lookup::Known(&v), "cn", "sn"),
                    Err(ScramError::ProofRejected)
                );
            }
        }
    }
}
