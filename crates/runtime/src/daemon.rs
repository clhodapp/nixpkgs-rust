//! A client for the worker protocol of the Nix daemon: enough to add the
//! store objects an evaluation decided on and to build derivations.
//!
//! Numbers are 64-bit little-endian; a string is its length followed by its
//! bytes, padded with zeros to a multiple of eight; a set of strings is its
//! size followed by the strings. Every request is answered by a stream of log
//! messages that ends with `STDERR_LAST` (or with an error), followed by the
//! reply.

use std::collections::BTreeSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;

const WORKER_MAGIC_1: u64 = 0x6e697863;
const WORKER_MAGIC_2: u64 = 0x6478696f;
/// Protocol 1.37, the last version before the feature exchange.
const PROTOCOL_VERSION: u64 = (1 << 8) | 37;

const STDERR_NEXT: u64 = 0x6f6c6d67;
const STDERR_READ: u64 = 0x64617461;
const STDERR_WRITE: u64 = 0x64617416;
const STDERR_LAST: u64 = 0x616c7473;
const STDERR_ERROR: u64 = 0x63787470;
const STDERR_START_ACTIVITY: u64 = 0x53545254;
const STDERR_STOP_ACTIVITY: u64 = 0x53544f50;
const STDERR_RESULT: u64 = 0x52534c54;

const OP_ADD_TO_STORE: u64 = 7;
const OP_ADD_TEXT_TO_STORE: u64 = 8;
const OP_BUILD_PATHS: u64 = 9;
const OP_ADD_INDIRECT_ROOT: u64 = 12;
const OP_QUERY_VALID_PATHS: u64 = 31;

/// Activity type of a build, whose start message names the derivation.
const ACT_BUILD: u64 = 105;

pub type DResult<T> = Result<T, String>;

/// An open connection to the daemon.
pub struct Daemon {
    r: BufReader<UnixStream>,
    w: BufWriter<UnixStream>,
    /// Whether to show the daemon's log lines on standard error.
    pub verbose: bool,
}

fn io(e: std::io::Error) -> String {
    format!("talking to the Nix daemon: {e}")
}

/// Sends data to the daemon in frames, as `AddToStore` reads it.
pub struct Framed<'a> {
    w: &'a mut BufWriter<UnixStream>,
    buf: Vec<u8>,
    err: Option<std::io::Error>,
}

impl Framed<'_> {
    fn flush_frame(&mut self) {
        if self.buf.is_empty() || self.err.is_some() {
            return;
        }
        let res = self
            .w
            .write_all(&(self.buf.len() as u64).to_le_bytes())
            .and_then(|_| self.w.write_all(&self.buf));
        if let Err(e) = res {
            self.err = Some(e);
        }
        self.buf.clear();
    }
}

impl crate::store::NarSink for Framed<'_> {
    fn write(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 1 << 16 {
            self.flush_frame();
        }
    }
}

impl Daemon {
    /// Connects to the daemon at `NIX_DAEMON_SOCKET_PATH`, or at the usual
    /// socket, and performs the handshake.
    pub fn connect() -> DResult<Daemon> {
        let path = std::env::var("NIX_DAEMON_SOCKET_PATH")
            .unwrap_or_else(|_| "/nix/var/nix/daemon-socket/socket".to_string());
        let s = UnixStream::connect(&path).map_err(|e| format!("cannot connect to the Nix daemon at '{path}': {e}"))?;
        let mut d = Daemon {
            r: BufReader::new(s.try_clone().map_err(io)?),
            w: BufWriter::new(s),
            verbose: false,
        };
        d.put(WORKER_MAGIC_1)?;
        d.put(PROTOCOL_VERSION)?;
        d.flush()?;
        if d.get()? != WORKER_MAGIC_2 {
            return Err("the Nix daemon speaks another protocol".to_string());
        }
        let daemon_version = d.get()?;
        if daemon_version >> 8 != 1 || (daemon_version & 0xff) < 37 {
            return Err(format!(
                "the Nix daemon speaks protocol {}.{}, but 1.37 or later is needed",
                daemon_version >> 8,
                daemon_version & 0xff
            ));
        }
        // Obsolete CPU affinity and space reservation.
        d.put(0)?;
        d.put(0)?;
        d.flush()?;
        let _nix_version = d.get_str()?;
        let _trusted = d.get()?;
        d.stderr()?;
        Ok(d)
    }

    fn put(&mut self, n: u64) -> DResult<()> {
        self.w.write_all(&n.to_le_bytes()).map_err(io)
    }

    fn put_bytes(&mut self, b: &[u8]) -> DResult<()> {
        self.put(b.len() as u64)?;
        self.w.write_all(b).map_err(io)?;
        let pad = (8 - b.len() % 8) % 8;
        self.w.write_all(&[0u8; 8][..pad]).map_err(io)
    }

    fn put_set(&mut self, s: &BTreeSet<String>) -> DResult<()> {
        self.put(s.len() as u64)?;
        for x in s {
            self.put_bytes(x.as_bytes())?;
        }
        Ok(())
    }

    fn flush(&mut self) -> DResult<()> {
        self.w.flush().map_err(io)
    }

    fn get(&mut self) -> DResult<u64> {
        let mut b = [0u8; 8];
        self.r.read_exact(&mut b).map_err(io)?;
        Ok(u64::from_le_bytes(b))
    }

    fn get_bytes(&mut self) -> DResult<Vec<u8>> {
        let n = self.get()? as usize;
        let mut b = vec![0u8; n];
        self.r.read_exact(&mut b).map_err(io)?;
        let pad = (8 - n % 8) % 8;
        let mut p = [0u8; 8];
        self.r.read_exact(&mut p[..pad]).map_err(io)?;
        Ok(b)
    }

    fn get_str(&mut self) -> DResult<String> {
        Ok(String::from_utf8_lossy(&self.get_bytes()?).into_owned())
    }

    fn get_set(&mut self) -> DResult<BTreeSet<String>> {
        let n = self.get()?;
        let mut s = BTreeSet::new();
        for _ in 0..n {
            s.insert(self.get_str()?);
        }
        Ok(s)
    }

    fn get_fields(&mut self) -> DResult<Vec<String>> {
        let n = self.get()?;
        let mut out = Vec::new();
        for _ in 0..n {
            match self.get()? {
                0 => out.push(self.get()?.to_string()),
                1 => out.push(self.get_str()?),
                t => return Err(format!("the Nix daemon sent a field of unknown type {t}")),
            }
        }
        Ok(out)
    }

    /// Reads log messages up to the end of the work for one request.
    fn stderr(&mut self) -> DResult<()> {
        self.flush()?;
        loop {
            match self.get()? {
                STDERR_LAST => return Ok(()),
                STDERR_NEXT => {
                    let s = self.get_str()?;
                    if self.verbose {
                        eprint!("{s}");
                    }
                }
                STDERR_START_ACTIVITY => {
                    let _id = self.get()?;
                    let _level = self.get()?;
                    let ty = self.get()?;
                    let s = self.get_str()?;
                    let _fields = self.get_fields()?;
                    let _parent = self.get()?;
                    if (self.verbose || ty == ACT_BUILD) && !s.is_empty() {
                        eprintln!("{s}");
                    }
                }
                STDERR_STOP_ACTIVITY => {
                    self.get()?;
                }
                STDERR_RESULT => {
                    let _id = self.get()?;
                    let ty = self.get()?;
                    let fields = self.get_fields()?;
                    // Result type 101 carries a line of build output.
                    if self.verbose && ty == 101 {
                        if let Some(line) = fields.first() {
                            eprintln!("{line}");
                        }
                    }
                }
                STDERR_ERROR => {
                    let _ty = self.get_str()?;
                    let _level = self.get()?;
                    let _name = self.get_str()?;
                    let msg = self.get_str()?;
                    let _have_pos = self.get()?;
                    let traces = self.get()?;
                    let mut out = msg;
                    for _ in 0..traces {
                        let _have_pos = self.get()?;
                        let t = self.get_str()?;
                        out = format!("{out}\n… {t}");
                    }
                    return Err(out);
                }
                STDERR_READ | STDERR_WRITE => {
                    return Err("the Nix daemon asked for a data transfer this client does not make".to_string());
                }
                m => return Err(format!("the Nix daemon sent an unknown message {m:#x}")),
            }
        }
    }

    /// The paths of `paths` that are valid in the store.
    pub fn query_valid_paths(&mut self, paths: &BTreeSet<String>) -> DResult<BTreeSet<String>> {
        self.put(OP_QUERY_VALID_PATHS)?;
        self.put_set(paths)?;
        self.put(0)?; // no substitution
        self.stderr()?;
        self.get_set()
    }

    /// Adds a text-hashed object (a `.drv` file or a `builtins.toFile` text)
    /// and returns its path.
    pub fn add_text(&mut self, name: &str, text: &[u8], refs: &BTreeSet<String>) -> DResult<String> {
        self.put(OP_ADD_TEXT_TO_STORE)?;
        self.put_bytes(name.as_bytes())?;
        self.put_bytes(text)?;
        self.put_set(refs)?;
        self.stderr()?;
        self.get_str()
    }

    /// Adds a content-addressed object. `method` is the content-address
    /// method with algorithm (`fixed:r:sha256` for an archive,
    /// `fixed:sha256` for a flat file), and `write` produces the data: an
    /// archive or the file's bytes. Returns the object's path.
    pub fn add_to_store(
        &mut self,
        name: &str,
        method: &str,
        refs: &BTreeSet<String>,
        write: &mut dyn FnMut(&mut dyn crate::store::NarSink) -> Result<(), String>,
    ) -> DResult<String> {
        self.put(OP_ADD_TO_STORE)?;
        self.put_bytes(name.as_bytes())?;
        self.put_bytes(method.as_bytes())?;
        self.put_set(refs)?;
        self.put(0)?; // no repair
        let res = {
            let mut f = Framed {
                w: &mut self.w,
                buf: Vec::new(),
                err: None,
            };
            let r = write(&mut f);
            f.flush_frame();
            match f.err {
                Some(e) => Err(io(e)),
                None => r,
            }
        };
        // The end of the data, sent even after an error so the connection
        // stays in step.
        self.put(0)?;
        res?;
        self.stderr()?;
        // The reply is the object's path info; the path comes first.
        let path = self.get_str()?;
        let _deriver = self.get_str()?;
        let _nar_hash = self.get_str()?;
        let _refs = self.get_set()?;
        let _reg_time = self.get()?;
        let _nar_size = self.get()?;
        let _ultimate = self.get()?;
        let _sigs = self.get_set()?;
        let _ca = self.get_str()?;
        Ok(path)
    }

    /// Builds derived paths (`<drv>!<outputs>`), building or substituting
    /// their dependencies first.
    pub fn build_paths(&mut self, paths: &[String]) -> DResult<()> {
        self.put(OP_BUILD_PATHS)?;
        self.put(paths.len() as u64)?;
        for p in paths {
            self.put_bytes(p.as_bytes())?;
        }
        self.put(0)?; // normal build mode
        self.stderr()?;
        self.get()?;
        Ok(())
    }

    /// Registers a symbolic link outside the store as a root of the garbage
    /// collector, as `nix-build` does for `./result`.
    pub fn add_indirect_root(&mut self, link: &str) -> DResult<()> {
        self.put(OP_ADD_INDIRECT_ROOT)?;
        self.put_bytes(link.as_bytes())?;
        self.stderr()?;
        self.get()?;
        Ok(())
    }
}
