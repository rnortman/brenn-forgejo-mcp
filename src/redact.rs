//! `RedactingWriter` — an `io::Write` adapter that replaces every
//! occurrence of configured secret byte sequences with `<REDACTED>` before
//! forwarding bytes to the inner writer. Occurrences that straddle a
//! `write` boundary are handled via a rolling carry. Note: `flush` does
//! NOT drain the carry (it may still be a prefix of a secret that
//! completes in the next write); the final carry is flushed on `Drop`.
//!
//! Also exposes [`scrub_bytes`] and [`scrub_str`] — one-shot scrubbers for
//! callers that already have a complete buffer (e.g. error strings before
//! they're embedded in a message that ships to stdout).

use std::io::{self, Write};
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretSlice, SecretString};

/// Placeholder that replaces each occurrence of a configured secret.
pub const REDACTION_PLACEHOLDER: &[u8] = b"<REDACTED>";

/// A shared, zeroize-on-drop set of secret byte sequences used for
/// scrubbing. Cloning is cheap (`Arc`); the underlying secret bytes live
/// exactly once.
///
/// Each stored secret is a `SecretSlice<u8>` which `ZeroizeOnDrop`s its
/// bytes when the last `Arc` is dropped.
///
/// The only ways to add a plaintext secret to a `SecretSet` are through
/// this module's `Builder` (which takes `&SecretString` or raw bytes) and
/// [`SecretSet::new`] / [`SecretSet::empty`]. The `Builder::push_secret_string`
/// entry point is the one the production `main.rs` uses — it takes
/// `&SecretString` directly and does the `expose_secret` call internally
/// so downstream code doesn't have to.
#[derive(Clone)]
pub struct SecretSet(Arc<Vec<SecretSlice<u8>>>);

/// Builder for [`SecretSet`]. Accumulates secrets from several sources and
/// finalizes them in one shot (sorted longest-first, empty entries dropped).
#[derive(Default)]
pub struct Builder {
    secrets: Vec<Vec<u8>>,
}

impl Builder {
    /// Add a PAT from a [`SecretString`] without requiring the caller to
    /// write `expose_secret()` themselves. This is the PAT-invariant
    /// boundary: the one place in the crate that intentionally materializes
    /// bytes from a `SecretString` for scrubbing purposes.
    pub fn push_secret_string(mut self, s: &SecretString) -> Self {
        self.secrets.push(s.expose_secret().as_bytes().to_vec());
        self
    }

    /// Add a raw byte secret. Used for the pre-logger startup path where
    /// we want to scrub plaintext extracted from a maybe-malformed TOML
    /// file before the real `ResolvedConfig` exists, and for tests.
    pub fn push_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.secrets.push(bytes);
        self
    }

    /// Finalize into a [`SecretSet`].
    pub fn build(self) -> SecretSet {
        SecretSet::new(self.secrets)
    }
}

impl SecretSet {
    /// Build a [`Builder`] that hides `expose_secret()` calls.
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Build from a list of byte secrets. Empty secrets are dropped.
    /// Stored longest-first so scans can match a longer secret before a
    /// shorter one that is its prefix.
    ///
    /// This entry point is for tests and for raw-bytes sources (where the
    /// caller already holds plaintext and isn't coming from a
    /// `SecretString`). Production code with a `SecretString` should use
    /// [`SecretSet::builder`] + [`Builder::push_secret_string`] instead.
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        let mut v: Vec<SecretSlice<u8>> = secrets
            .into_iter()
            .map(|s| s.into())
            .filter(|s| !s.is_empty())
            .map(SecretSlice::from)
            .collect();
        // Longest first. The length of a secret is not itself secret; we
        // call `expose_secret` here only to read `.len()`.
        v.sort_by_key(|s| std::cmp::Reverse(s.expose_secret().len()));
        SecretSet(Arc::new(v))
    }

    /// Empty set (no secrets configured).
    pub fn empty() -> Self {
        SecretSet(Arc::new(Vec::new()))
    }

    /// Largest secret length in the set, or 0 if empty. Calls
    /// `expose_secret` only to read `.len()`.
    fn max_len(&self) -> usize {
        self.0
            .iter()
            .map(|s| s.expose_secret().len())
            .max()
            .unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the secret slices as `&[&[u8]]`. Uses `expose_secret` under
    /// the hood for the scan routines in this module; the returned refs
    /// must not be stored anywhere that outlives the scan.
    fn slices(&self) -> Vec<&[u8]> {
        self.0.iter().map(|s| s.expose_secret()).collect()
    }
}

/// Decision at a given scan position, returned by [`decide`].
enum Step {
    /// Found a definite match; emit `<REDACTED>` and advance by `n`.
    Match { n: usize },
    /// Need more input before we can decide (only possible when
    /// `final_pass == false`). The caller stops the scan here.
    Hold,
    /// No match and no further input could produce one; emit the byte and
    /// advance by 1.
    Pass,
}

/// Decide what to do at position `i` in `buf`:
///
/// - `match_len`: the length of the longest secret that fully matches
///   `buf[i..]`, if any.
/// - `prefix_len`: the length of the longest secret for which `buf[i..]`
///   is a *proper* prefix (i.e. `buf.len() - i < secret.len()` and
///   `secret.starts_with(&buf[i..])`).
///
/// Rules:
///
/// - If `final_pass` is true, we know no more bytes will arrive, so
///   prefixes can never complete → ignore them.
/// - Otherwise, if some secret could still extend into a longer match than
///   we've seen so far, hold (so the caller carries the tail forward).
/// - Otherwise, emit a match if we have one, else pass the byte through.
fn decide(buf: &[u8], i: usize, secrets: &[&[u8]], final_pass: bool) -> Step {
    let tail = &buf[i..];
    let mut match_len: usize = 0;
    let mut prefix_len: usize = 0;
    for sec in secrets {
        if tail.len() >= sec.len() {
            if &tail[..sec.len()] == *sec && sec.len() > match_len {
                match_len = sec.len();
            }
        } else if !tail.is_empty() && sec.starts_with(tail) && sec.len() > prefix_len {
            prefix_len = sec.len();
        }
    }
    if !final_pass && prefix_len > match_len {
        Step::Hold
    } else if match_len > 0 {
        Step::Match { n: match_len }
    } else {
        Step::Pass
    }
}

/// Scan `buf` and return `(scrubbed_output, bytes_committed)`.
///
/// When `final_pass == true`, `bytes_committed == buf.len()` and every byte
/// has been emitted (redacted or passed-through). When `final_pass ==
/// false`, any trailing "could-be-a-prefix" bytes are left uncommitted —
/// the caller is expected to carry them forward for the next scan.
fn scrub_with(buf: &[u8], secrets: &[&[u8]], final_pass: bool) -> (Vec<u8>, usize) {
    let mut out: Vec<u8> = Vec::with_capacity(buf.len());
    let mut i = 0usize;
    let mut committed = 0usize;
    while i < buf.len() {
        match decide(buf, i, secrets, final_pass) {
            Step::Match { n } => {
                out.extend_from_slice(REDACTION_PLACEHOLDER);
                i += n;
                committed = i;
            }
            Step::Hold => {
                // Leave buf[i..] for the next write.
                break;
            }
            Step::Pass => {
                out.push(buf[i]);
                i += 1;
                committed = i;
            }
        }
    }
    (out, committed)
}

/// One-shot scrub: replace every occurrence of any configured secret with
/// [`REDACTION_PLACEHOLDER`] and return the scrubbed bytes. No carry, no
/// streaming — for callers that already have a complete buffer.
pub fn scrub_bytes(buf: &[u8], secrets: &SecretSet) -> Vec<u8> {
    if secrets.is_empty() {
        return buf.to_vec();
    }
    let slices = secrets.slices();
    let (out, _) = scrub_with(buf, &slices, true);
    out
}

/// One-shot scrub for UTF-8 strings. If no secret is present, returns the
/// input borrowed. Otherwise returns an owned `String` — note that scrubbing
/// is applied to the raw bytes; if a secret crosses an interior UTF-8
/// boundary the output might not be valid UTF-8, in which case we fall back
/// to `String::from_utf8_lossy`.
pub fn scrub_str<'a>(s: &'a str, secrets: &SecretSet) -> std::borrow::Cow<'a, str> {
    if secrets.is_empty() {
        return std::borrow::Cow::Borrowed(s);
    }
    let scrubbed = scrub_bytes(s.as_bytes(), secrets);
    if scrubbed == s.as_bytes() {
        std::borrow::Cow::Borrowed(s)
    } else {
        match String::from_utf8(scrubbed) {
            Ok(s) => std::borrow::Cow::Owned(s),
            Err(e) => {
                std::borrow::Cow::Owned(String::from_utf8_lossy(&e.into_bytes()).into_owned())
            }
        }
    }
}

/// Wraps an inner writer and replaces every occurrence of any configured
/// secret byte sequence with [`REDACTION_PLACEHOLDER`] before forwarding
/// bytes to the inner writer. Empty secrets are silently ignored.
pub struct RedactingWriter<W: Write> {
    inner: W,
    secrets: SecretSet,
    /// Largest secret length; cached because it bounds the carry size.
    max_secret_len: usize,
    /// Rolling carry buffer for boundary-straddling secrets.
    /// Invariant: `carry.len() < max_secret_len` after each `write`.
    carry: Vec<u8>,
}

impl<W: Write> RedactingWriter<W> {
    /// Build a redactor. Secrets are taken from the shared [`SecretSet`].
    pub fn new(inner: W, secrets: SecretSet) -> Self {
        let max_secret_len = secrets.max_len();
        let carry = if max_secret_len == 0 {
            Vec::new()
        } else {
            Vec::with_capacity(max_secret_len.saturating_sub(1))
        };
        Self {
            inner,
            secrets,
            max_secret_len,
            carry,
        }
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.max_secret_len == 0 {
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        }

        let mut combined = Vec::with_capacity(self.carry.len().saturating_add(buf.len()));
        combined.extend_from_slice(&self.carry);
        combined.extend_from_slice(buf);

        let slices = self.secrets.slices();
        let (scrubbed, committed) = scrub_with(&combined, &slices, false);
        self.inner.write_all(&scrubbed)?;

        // Retain anything uncommitted as the new carry.
        self.carry.clear();
        self.carry.extend_from_slice(&combined[committed..]);
        // The carry is always strictly shorter than the longest secret:
        // a full-length match would have been committed.
        debug_assert!(self.carry.len() < self.max_secret_len);

        // Upstream writers (e.g. env_logger) expect `Ok(buf.len())` on
        // success — the caller's buffer was fully accepted.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Do NOT flush the carry: it might still be the prefix of a secret
        // that completes in the next write. Drop flushes.
        self.inner.flush()
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        if !self.carry.is_empty() {
            let carry = std::mem::take(&mut self.carry);
            let slices = self.secrets.slices();
            let (out, _) = scrub_with(&carry, &slices, true);
            // Drop has no error channel; a stderr write failure at process
            // exit is not recoverable.
            let _ = self.inner.write_all(&out);
            let _ = self.inner.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `chunks` sequentially through a `RedactingWriter` with the
    /// given `secrets`, drop the writer (triggering final-flush of the
    /// carry), and return the bytes that ended up in the inner buffer.
    fn run_chunks_ref(secrets: &[&[u8]], chunks: &[&[u8]]) -> Vec<u8> {
        let mut inner: Vec<u8> = Vec::new();
        {
            let ss = SecretSet::new(secrets.iter().map(|s| s.to_vec()));
            let mut w = RedactingWriter::new(&mut inner, ss);
            for c in chunks {
                w.write_all(c).unwrap();
            }
            // `w` dropped here → drop() flushes the carry into `inner`.
        }
        inner
    }

    #[test]
    fn single_chunk_single_secret_replaced() {
        let out = run_chunks_ref(&[b"PAT"], &[b"hello PAT world"]);
        assert_eq!(out, b"hello <REDACTED> world");
    }

    #[test]
    fn multiple_occurrences_in_one_chunk() {
        let out = run_chunks_ref(&[b"PAT"], &[b"PAT then PAT and PAT"]);
        assert_eq!(out, b"<REDACTED> then <REDACTED> and <REDACTED>");
    }

    #[test]
    fn secret_straddling_write_boundary() {
        let out = run_chunks_ref(&[b"SECRETTOKEN"], &[b"before SECRET", b"TOKEN after"]);
        assert_eq!(out, b"before <REDACTED> after");
    }

    #[test]
    fn secret_at_start_of_buffer() {
        let out = run_chunks_ref(&[b"PAT"], &[b"PAT at start"]);
        assert_eq!(out, b"<REDACTED> at start");
    }

    #[test]
    fn secret_at_end_of_buffer_flushed_on_drop() {
        let out = run_chunks_ref(&[b"PAT"], &[b"end with PAT"]);
        assert_eq!(out, b"end with <REDACTED>");
    }

    #[test]
    fn secret_alone_in_buffer() {
        let out = run_chunks_ref(&[b"HIDE_ME"], &[b"HIDE_ME"]);
        assert_eq!(out, b"<REDACTED>");
    }

    #[test]
    fn no_secret_passes_through_unchanged() {
        let out = run_chunks_ref(&[b"PAT"], &[b"nothing to redact here"]);
        assert_eq!(out, b"nothing to redact here");
    }

    #[test]
    fn empty_secret_ignored() {
        let out = run_chunks_ref(&[b""], &[b"hello"]);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn straddle_three_writes() {
        let out = run_chunks_ref(&[b"ABCDEF"], &[b"xyz AB", b"CD", b"EF end"]);
        assert_eq!(out, b"xyz <REDACTED> end");
    }

    #[test]
    fn write_returns_original_buf_len() {
        let mut inner: Vec<u8> = Vec::new();
        let ss = SecretSet::new([b"PAT".to_vec()]);
        let mut w = RedactingWriter::new(&mut inner, ss);
        let n = w.write(b"PAT").unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn flush_forwards_but_retains_carry() {
        let mut inner: Vec<u8> = Vec::new();
        {
            let ss = SecretSet::new([b"SECRET".to_vec()]);
            let mut w = RedactingWriter::new(&mut inner, ss);
            w.write_all(b"hello SEC").unwrap();
            w.flush().unwrap();
            w.write_all(b"RET bye").unwrap();
        }
        assert_eq!(inner, b"hello <REDACTED> bye");
    }

    #[test]
    fn multiple_secrets_both_matched() {
        let out = run_chunks_ref(&[b"ALPHA", b"BETA"], &[b"pre ALPHA mid BETA post"]);
        assert_eq!(out, b"pre <REDACTED> mid <REDACTED> post");
    }

    #[test]
    fn longer_secret_wins_when_overlapping() {
        // "ABCD" and "AB" — the longer must win when both are possible.
        let out = run_chunks_ref(&[b"AB", b"ABCD"], &[b"x ABCD y"]);
        assert_eq!(out, b"x <REDACTED> y");
    }

    #[test]
    fn secret_at_very_end_exactly_one_write() {
        let out = run_chunks_ref(&[b"ZZ"], &[b"aZZ"]);
        assert_eq!(out, b"a<REDACTED>");
    }

    /// Regression for F11: when "AB" and "ABCD" are both configured, a
    /// `write_all(b"AB")` at a boundary must not commit "AB" — a longer
    /// secret could still match on the next write.
    #[test]
    fn longer_secret_wins_across_write_boundary() {
        let out = run_chunks_ref(&[b"AB", b"ABCD"], &[b"AB", b"CD"]);
        assert_eq!(out, b"<REDACTED>");
    }

    /// Same idea with a suffix after the boundary.
    #[test]
    fn longer_secret_wins_across_boundary_with_trailing_text() {
        let out = run_chunks_ref(&[b"AB", b"ABCD"], &[b"pre AB", b"CD post"]);
        assert_eq!(out, b"pre <REDACTED> post");
    }

    /// If the shorter secret appears alone (no longer match possible), it
    /// still gets redacted.
    #[test]
    fn shorter_secret_alone_still_redacted() {
        // "AB" on its own, terminated by non-extending byte "X".
        let out = run_chunks_ref(&[b"AB", b"ABCD"], &[b"AB", b"X end"]);
        assert_eq!(out, b"<REDACTED>X end");
    }

    /// `scrub_bytes` one-shot scrubs a complete buffer. This is the path
    /// error-string scrubbing uses.
    #[test]
    fn scrub_bytes_replaces_occurrences() {
        let ss = SecretSet::new([b"PAT".to_vec()]);
        let out = scrub_bytes(b"here is PAT and PAT", &ss);
        assert_eq!(out, b"here is <REDACTED> and <REDACTED>");
    }

    #[test]
    fn scrub_bytes_empty_secret_set_is_identity() {
        let ss = SecretSet::empty();
        let out = scrub_bytes(b"nothing here", &ss);
        assert_eq!(out, b"nothing here");
    }

    #[test]
    fn scrub_str_no_secret_borrows() {
        let ss = SecretSet::new([b"PAT".to_vec()]);
        let s = "no secret here";
        let out = scrub_str(s, &ss);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn scrub_str_with_secret_owns() {
        let ss = SecretSet::new([b"PAT".to_vec()]);
        let out = scrub_str("contains PAT bytes", &ss);
        assert_eq!(&*out, "contains <REDACTED> bytes");
    }
}
