//! Writing NetCDF classic files, 64-bit offset variant (CDF-2), as specified in
//! "The NetCDF Classic Format Specification" (docs.unidata.ucar.edu). Just what the
//! OpenSense export needs: fixed-size dimensions, text and scalar number attributes, and
//! char, float and double variables. Everything is big-endian and padded to 4 bytes.

use anyhow::{Result, bail};

const NC_DIMENSION: u32 = 10;
const NC_VARIABLE: u32 = 11;
const NC_ATTRIBUTE: u32 = 12;
const NC_CHAR: u32 = 2;
const NC_FLOAT: u32 = 5;
const NC_DOUBLE: u32 = 6;

/// The library's default fill values, which tools treat as missing.
pub const FILL_FLOAT: f32 = 9.969_21e36;
pub const FILL_DOUBLE: f64 = 9.969_209_968_386_869e36;

#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    Text(String),
    Float(f32),
    Double(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Values {
    Char(Vec<u8>),
    Float(Vec<f32>),
    Double(Vec<f64>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Var {
    pub name: String,
    /// indexes into the file's dimensions, outermost first
    pub dims: Vec<usize>,
    pub attrs: Vec<(String, Attr)>,
    pub values: Values,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct File {
    pub dims: Vec<(String, usize)>,
    pub attrs: Vec<(String, Attr)>,
    pub vars: Vec<Var>,
}

impl Values {
    fn len(&self) -> usize {
        match self {
            Values::Char(v) => v.len(),
            Values::Float(v) => v.len(),
            Values::Double(v) => v.len(),
        }
    }

    fn code(&self) -> u32 {
        match self {
            Values::Char(_) => NC_CHAR,
            Values::Float(_) => NC_FLOAT,
            Values::Double(_) => NC_DOUBLE,
        }
    }

    fn put(&self, b: &mut Vec<u8>) {
        match self {
            Values::Char(v) => b.extend_from_slice(v),
            Values::Float(v) => v.iter().for_each(|x| b.extend(x.to_be_bytes())),
            Values::Double(v) => v.iter().for_each(|x| b.extend(x.to_be_bytes())),
        }
        pad(b);
    }

    fn padded_size(&self) -> usize {
        let raw = match self {
            Values::Char(v) => v.len(),
            Values::Float(v) => 4 * v.len(),
            Values::Double(v) => 8 * v.len(),
        };
        raw.div_ceil(4) * 4
    }
}

fn pad(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}

fn u32be(b: &mut Vec<u8>, v: usize) {
    b.extend((v as u32).to_be_bytes());
}

fn name(b: &mut Vec<u8>, s: &str) {
    u32be(b, s.len());
    b.extend_from_slice(s.as_bytes());
    pad(b);
}

fn attrs(b: &mut Vec<u8>, list: &[(String, Attr)]) {
    if list.is_empty() {
        b.extend([0; 8]);
        return;
    }
    u32be(b, NC_ATTRIBUTE as usize);
    u32be(b, list.len());
    for (n, a) in list {
        name(b, n);
        let v = match a {
            Attr::Text(t) => Values::Char(t.as_bytes().to_vec()),
            Attr::Float(x) => Values::Float(vec![*x]),
            Attr::Double(x) => Values::Double(vec![*x]),
        };
        u32be(b, v.code() as usize);
        u32be(b, v.len());
        v.put(b);
    }
}

/// NetCDF names may hold most characters, but not '/', control characters, or a
/// leading or trailing space.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(' ')
        && !n.ends_with(' ')
        && !n.contains('/')
        && !n.chars().any(char::is_control)
}

impl File {
    fn check(&self) -> Result<()> {
        let names = self
            .dims
            .iter()
            .map(|d| &d.0)
            .chain(self.attrs.iter().map(|a| &a.0))
            .chain(self.vars.iter().map(|v| &v.name))
            .chain(self.vars.iter().flat_map(|v| v.attrs.iter().map(|a| &a.0)));
        for n in names {
            if !valid_name(n) {
                bail!("{n:?} is not a valid NetCDF name");
            }
        }
        if let Some((n, _)) = self.dims.iter().find(|d| d.1 == 0) {
            // a zero length marks the record dimension, which this writer doesn't do
            bail!("dimension {n} is empty");
        }
        for v in &self.vars {
            let mut n = 1usize;
            for &d in &v.dims {
                let Some((_, len)) = self.dims.get(d) else {
                    bail!("variable {} uses a dimension that doesn't exist", v.name);
                };
                n *= len;
            }
            if n != v.values.len() {
                bail!(
                    "variable {} has {} values for {n} cells",
                    v.name,
                    v.values.len()
                );
            }
            if v.values.padded_size() > u32::MAX as usize - 3 {
                bail!("variable {} is too big for a classic NetCDF file", v.name);
            }
        }
        Ok(())
    }

    fn header(&self, begins: &[u64]) -> Vec<u8> {
        let mut b = b"CDF\x02".to_vec();
        u32be(&mut b, 0);
        if self.dims.is_empty() {
            b.extend([0; 8]);
        } else {
            u32be(&mut b, NC_DIMENSION as usize);
            u32be(&mut b, self.dims.len());
            for (n, len) in &self.dims {
                name(&mut b, n);
                u32be(&mut b, *len);
            }
        }
        attrs(&mut b, &self.attrs);
        if self.vars.is_empty() {
            b.extend([0; 8]);
        } else {
            u32be(&mut b, NC_VARIABLE as usize);
            u32be(&mut b, self.vars.len());
            for (v, begin) in self.vars.iter().zip(begins) {
                name(&mut b, &v.name);
                u32be(&mut b, v.dims.len());
                for &d in &v.dims {
                    u32be(&mut b, d);
                }
                attrs(&mut b, &v.attrs);
                u32be(&mut b, v.values.code() as usize);
                u32be(&mut b, v.values.padded_size());
                b.extend(begin.to_be_bytes());
            }
        }
        b
    }

    /// The whole file.
    pub fn write(&self) -> Result<Vec<u8>> {
        self.check()?;
        let mut at = self.header(&vec![0; self.vars.len()]).len() as u64;
        let begins: Vec<u64> = self
            .vars
            .iter()
            .map(|v| {
                let b = at;
                at += v.values.padded_size() as u64;
                b
            })
            .collect();
        let mut out = self.header(&begins);
        out.reserve(at as usize - out.len());
        for v in &self.vars {
            v.values.put(&mut out);
        }
        Ok(out)
    }
}

/// A reader for the same subset, to check what the writer wrote.
#[cfg(test)]
pub mod read {
    use super::*;

    struct Cur<'a> {
        b: &'a [u8],
        at: usize,
    }

    impl Cur<'_> {
        fn take(&mut self, n: usize) -> &[u8] {
            let s = &self.b[self.at..self.at + n];
            self.at += n;
            s
        }
        fn u32(&mut self) -> usize {
            u32::from_be_bytes(self.take(4).try_into().unwrap()) as usize
        }
        fn u64(&mut self) -> usize {
            u64::from_be_bytes(self.take(8).try_into().unwrap()) as usize
        }
        fn skip_pad(&mut self) {
            self.at = self.at.div_ceil(4) * 4;
        }
        fn name(&mut self) -> String {
            let n = self.u32();
            let s = String::from_utf8(self.take(n).to_vec()).unwrap();
            self.skip_pad();
            s
        }
        fn values(&mut self, code: u32, n: usize) -> Values {
            let v = match code {
                NC_CHAR => Values::Char(self.take(n).to_vec()),
                NC_FLOAT => Values::Float(
                    self.take(4 * n)
                        .chunks(4)
                        .map(|c| f32::from_be_bytes(c.try_into().unwrap()))
                        .collect(),
                ),
                NC_DOUBLE => Values::Double(
                    self.take(8 * n)
                        .chunks(8)
                        .map(|c| f64::from_be_bytes(c.try_into().unwrap()))
                        .collect(),
                ),
                other => panic!("type {other} not supported"),
            };
            self.skip_pad();
            v
        }
        fn list(&mut self, tag: u32) -> usize {
            let t = self.u32();
            let n = self.u32();
            assert!(t == tag as usize || (t == 0 && n == 0), "bad list tag {t}");
            n
        }
        fn attrs(&mut self) -> Vec<(String, Attr)> {
            (0..self.list(NC_ATTRIBUTE))
                .map(|_| {
                    let n = self.name();
                    let code = self.u32() as u32;
                    let len = self.u32();
                    let a = match self.values(code, len) {
                        Values::Char(t) => Attr::Text(String::from_utf8(t).unwrap()),
                        Values::Float(v) => Attr::Float(v[0]),
                        Values::Double(v) => Attr::Double(v[0]),
                    };
                    (n, a)
                })
                .collect()
        }
    }

    pub fn file(b: &[u8]) -> File {
        assert_eq!(&b[..4], b"CDF\x02");
        let mut c = Cur { b, at: 4 };
        assert_eq!(c.u32(), 0, "no records");
        let dims: Vec<(String, usize)> = (0..c.list(NC_DIMENSION))
            .map(|_| (c.name(), c.u32()))
            .collect();
        let attrs = c.attrs();
        let mut vars = vec![];
        for _ in 0..c.list(NC_VARIABLE) {
            let name = c.name();
            let vd: Vec<usize> = (0..c.u32()).map(|_| c.u32()).collect();
            let vattrs = c.attrs();
            let code = c.u32() as u32;
            let _vsize = c.u32();
            let begin = c.u64();
            let n: usize = vd.iter().map(|&d| dims[d].1).product();
            let mut at = Cur { b, at: begin };
            vars.push(Var {
                name,
                dims: vd,
                attrs: vattrs,
                values: at.values(code, n),
            });
        }
        File { dims, attrs, vars }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tiny_file_has_the_bytes_the_spec_describes() {
        let f = File {
            dims: vec![("x".into(), 2)],
            attrs: vec![],
            vars: vec![Var {
                name: "v".into(),
                dims: vec![0],
                attrs: vec![],
                values: Values::Float(vec![1.0, 2.0]),
            }],
        };
        let mut want = b"CDF\x02".to_vec();
        for w in [0u32, 10, 1, 1] {
            want.extend(w.to_be_bytes());
        }
        want.extend(b"x\0\0\0");
        want.extend(2u32.to_be_bytes());
        want.extend([0; 8]); // no global attributes
        for w in [11u32, 1, 1] {
            want.extend(w.to_be_bytes());
        }
        want.extend(b"v\0\0\0");
        for w in [1u32, 0] {
            want.extend(w.to_be_bytes());
        }
        want.extend([0; 8]); // no variable attributes
        for w in [5u32, 8] {
            want.extend(w.to_be_bytes());
        }
        want.extend(84u64.to_be_bytes()); // data starts right after this 84-byte header
        want.extend(1.0f32.to_be_bytes());
        want.extend(2.0f32.to_be_bytes());
        assert_eq!(f.write().unwrap(), want);
    }

    #[test]
    fn writes_and_reads_back_text_floats_and_doubles() {
        let f = File {
            dims: vec![("t".into(), 3), ("id".into(), 2), ("len".into(), 5)],
            attrs: vec![
                ("title".into(), Attr::Text("odd length".into())),
                (
                    "naming convention".into(),
                    Attr::Text("OpenSense-CML".into()),
                ),
            ],
            vars: vec![
                Var {
                    name: "id".into(),
                    dims: vec![1, 2],
                    attrs: vec![("long_name".into(), Attr::Text("ids".into()))],
                    values: Values::Char(b"ab\0\0\0cde\0\0".to_vec()),
                },
                Var {
                    name: "rsl".into(),
                    dims: vec![1, 0],
                    attrs: vec![("_FillValue".into(), Attr::Float(FILL_FLOAT))],
                    values: Values::Float(vec![-60.0, FILL_FLOAT, -61.5, -70.0, -71.0, -72.0]),
                },
                Var {
                    name: "time".into(),
                    dims: vec![0],
                    attrs: vec![("scale".into(), Attr::Double(0.5))],
                    values: Values::Double(vec![1.795e9, 1.795e9 + 60.0, 1.795e9 + 120.0]),
                },
            ],
        };
        let bytes = f.write().unwrap();
        assert_eq!(bytes.len() % 4, 0);
        assert_eq!(read::file(&bytes), f);
    }

    #[test]
    fn refuses_mismatched_sizes_and_bad_names() {
        let mut f = File {
            dims: vec![("x".into(), 2)],
            attrs: vec![],
            vars: vec![Var {
                name: "v".into(),
                dims: vec![0],
                attrs: vec![],
                values: Values::Double(vec![1.0]),
            }],
        };
        assert!(f.write().is_err());
        f.vars[0].values = Values::Double(vec![1.0, 2.0]);
        f.vars[0].name = "a/b".into();
        assert!(f.write().is_err());
        f.vars[0].name = "ok".into();
        f.dims[0].1 = 0;
        assert!(f.write().is_err());
    }
}
