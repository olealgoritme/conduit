//! Reading a trace in either format, from a file or a live stream.

use crate::json::{self, Line};
use crate::{HEADER_LEN, Header, MAGIC, RECORD_LEN, Record};
use std::io::{self, BufRead};

/// The two encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// JSON Lines, the default.
    Json,
    /// Fixed 72-byte records after a 32-byte header.
    Binary,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" | "jsonl" => Some(Format::Json),
            "bin" | "binary" => Some(Format::Binary),
            _ => None,
        }
    }

    /// The format a file name suggests: `.bin` is binary, anything else JSON.
    pub fn for_path(p: &std::path::Path) -> Self {
        match p.extension().and_then(|e| e.to_str()) {
            Some("bin") => Format::Binary,
            _ => Format::Json,
        }
    }
}

/// Reads records, whichever format the input turns out to be in.
pub struct Reader<R> {
    inner: R,
    format: Format,
    header: Header,
    pending: Option<Record>,
    line: String,
    lineno: usize,
}

impl<R: BufRead> Reader<R> {
    /// Look at the first bytes to tell the formats apart, and read the header.
    pub fn new(mut inner: R) -> io::Result<Self> {
        let starts_binary = {
            let buf = inner.fill_buf()?;
            buf.len() >= MAGIC.len() && buf[..MAGIC.len()] == MAGIC
                || (!buf.is_empty() && buf.len() < MAGIC.len() && MAGIC.starts_with(buf))
        };
        let mut r = Reader {
            inner,
            format: if starts_binary {
                Format::Binary
            } else {
                Format::Json
            },
            header: Header::default(),
            pending: None,
            line: String::new(),
            lineno: 0,
        };
        match r.format {
            Format::Binary => {
                let mut h = [0u8; HEADER_LEN];
                r.inner.read_exact(&mut h)?;
                r.header = Header::from_bytes(&h).map_err(io::Error::other)?;
            }
            Format::Json => {
                // The header line is optional: a trace cut out of a larger
                // one with grep is still a trace.
                if let Some(line) = r.json_line()? {
                    match line {
                        Line::Header(h) => r.header = h,
                        Line::Record(rec) => r.pending = Some(rec),
                    }
                }
            }
        }
        Ok(r)
    }

    pub fn header(&self) -> Header {
        self.header
    }

    pub fn format(&self) -> Format {
        self.format
    }

    fn json_line(&mut self) -> io::Result<Option<Line>> {
        loop {
            self.line.clear();
            if self.inner.read_line(&mut self.line)? == 0 {
                return Ok(None);
            }
            self.lineno += 1;
            if self.line.trim().is_empty() {
                continue;
            }
            return json::parse_line(&self.line)
                .map(Some)
                .map_err(|e| io::Error::other(format!("line {}: {e}", self.lineno)));
        }
    }

    /// The next record, or `None` at the end. A binary stream cut off in the
    /// middle of a record ends there, as a file still being written does.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        if let Some(r) = self.pending.take() {
            return Ok(Some(r));
        }
        match self.format {
            Format::Binary => {
                let mut b = [0u8; RECORD_LEN];
                match self.inner.read_exact(&mut b) {
                    Ok(()) => Record::from_bytes(&b)
                        .map(Some)
                        .ok_or_else(|| io::Error::other("a record this build cannot read")),
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Format::Json => loop {
                match self.json_line()? {
                    None => return Ok(None),
                    Some(Line::Record(r)) => return Ok(Some(r)),
                    // A second header: two traces appended to one file.
                    Some(Line::Header(h)) => self.header = h,
                }
            },
        }
    }
}

impl<R: BufRead> Iterator for Reader<R> {
    type Item = io::Result<Record>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

/// Write records in either format. JSON needs the release for NVKMS names.
pub struct Writer<W> {
    inner: W,
    format: Format,
    header: Header,
    buf: String,
}

impl<W: io::Write> Writer<W> {
    /// Start a trace: the header goes out at once.
    pub fn new(mut inner: W, format: Format, header: Header) -> io::Result<Self> {
        match format {
            Format::Binary => inner.write_all(&header.to_bytes())?,
            Format::Json => writeln!(inner, "{}", json::header_line(&header))?,
        }
        Ok(Writer {
            inner,
            format,
            header,
            buf: String::new(),
        })
    }

    pub fn write(&mut self, r: &Record) -> io::Result<()> {
        match self.format {
            Format::Binary => self.inner.write_all(&r.to_bytes()),
            Format::Json => {
                self.buf.clear();
                json::write_record(&mut self.buf, r, self.header.driver);
                self.inner.write_all(self.buf.as_bytes())
            }
        }
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::sample;
    use crate::{Call, DriverVersion};

    fn round_trip(format: Format) {
        let header = Header {
            driver: Some(DriverVersion::new(580, 178, 4)),
        };
        let recs: Vec<Record> = (0..50)
            .map(|i| Record {
                ts_ns: 1_000 + i,
                call: if i % 2 == 0 {
                    Call::Control
                } else {
                    Call::Alloc
                },
                ..sample()
            })
            .collect();
        let mut w = Writer::new(Vec::new(), format, header).unwrap();
        for r in &recs {
            w.write(r).unwrap();
        }
        let bytes = w.get_mut().clone();
        let mut rd = Reader::new(&bytes[..]).unwrap();
        assert_eq!(rd.format(), format);
        assert_eq!(rd.header(), header);
        let back: Vec<Record> = rd.by_ref().map(Result::unwrap).collect();
        assert_eq!(back, recs);
    }

    #[test]
    fn json_files_read_back() {
        round_trip(Format::Json);
    }

    #[test]
    fn binary_files_read_back() {
        round_trip(Format::Binary);
    }

    #[test]
    fn a_binary_file_cut_mid_record_ends_cleanly() {
        let mut w = Writer::new(Vec::new(), Format::Binary, Header::default()).unwrap();
        w.write(&sample()).unwrap();
        w.write(&sample()).unwrap();
        let mut bytes = w.get_mut().clone();
        bytes.truncate(bytes.len() - 10);
        let back: Vec<_> = Reader::new(&bytes[..]).unwrap().collect();
        assert_eq!(back.len(), 1);
    }

    #[test]
    fn json_without_a_header_still_reads() {
        let mut s = String::new();
        crate::json::write_record(&mut s, &sample(), None);
        let back: Vec<_> = Reader::new(s.as_bytes())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(back, vec![sample()]);
    }

    #[test]
    fn the_format_follows_the_extension() {
        use std::path::Path;
        assert_eq!(Format::for_path(Path::new("t.bin")), Format::Binary);
        assert_eq!(Format::for_path(Path::new("t.jsonl")), Format::Json);
        assert_eq!(Format::for_path(Path::new("t")), Format::Json);
    }
}
