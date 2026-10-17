//! Stream remote output while rewriting remote paths to local ones.

use std::io::{self, Read, Write};

/// Copies `input` to `output` segment by segment (split on `\n` and `\r`, so
/// cargo's progress bar keeps updating), replacing `from` with `to`.
pub fn rewrite_stream(
    mut input: impl Read,
    mut output: impl Write,
    from: &[u8],
    to: &[u8],
) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        if let Some(end) = pending.iter().rposition(|b| *b == b'\n' || *b == b'\r') {
            let rest = pending.split_off(end + 1);
            output.write_all(&replace(&pending, from, to))?;
            output.flush()?;
            pending = rest;
        }
    }
    output.write_all(&replace(&pending, from, to))?;
    output.flush()
}

pub fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands out data in small chunks to exercise boundary handling.
    struct Chunked<'a>(&'a [u8], usize);

    impl Read for Chunked<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.len().min(self.1).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn rewrites_paths_split_across_reads() {
        let input = b"Compiling foo (/r/proj)\r  Building\n{\"src\":\"/r/proj/src/main.rs\"}";
        let mut out = Vec::new();
        rewrite_stream(Chunked(input, 3), &mut out, b"/r/proj", b"/home/me/proj").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Compiling foo (/home/me/proj)\r  Building\n{\"src\":\"/home/me/proj/src/main.rs\"}"
        );
    }
}
