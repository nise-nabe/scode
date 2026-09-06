//! Name dictionary (string ↔ id).

use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct Dictionary {
    to_id: HashMap<String, u32>,
    to_str: Vec<String>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.to_id.get(s) {
            return id;
        }
        let id = self.to_str.len() as u32;
        self.to_str.push(s.to_string());
        self.to_id.insert(s.to_string(), id);
        id
    }

    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.to_id.get(s).copied()
    }

    pub fn resolve(&self, id: u32) -> Option<&str> {
        self.to_str.get(id as usize).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.to_str.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_str.is_empty()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.to_str.len() as u32).to_le_bytes());
        for s in &self.to_str {
            let b = s.as_bytes();
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    pub fn from_bytes(mut data: &[u8]) -> anyhow::Result<Self> {
        let n = read_u32(&mut data)? as usize;
        let mut dict = Dictionary::new();
        dict.to_str.reserve(n);
        for _ in 0..n {
            let len = read_u32(&mut data)? as usize;
            if data.len() < len {
                anyhow::bail!("truncated dictionary string");
            }
            let s = std::str::from_utf8(&data[..len])?.to_string();
            data = &data[len..];
            let id = dict.to_str.len() as u32;
            if dict.to_id.insert(s.clone(), id).is_some() {
                anyhow::bail!("corrupt dictionary: duplicate string `{s}`");
            }
            dict.to_str.push(s);
        }
        if !data.is_empty() {
            anyhow::bail!(
                "corrupt dictionary: {} trailing byte(s) after {} entries",
                data.len(),
                n
            );
        }
        Ok(dict)
    }
}

fn read_u32(data: &mut &[u8]) -> anyhow::Result<u32> {
    if data.len() < 4 {
        anyhow::bail!("truncated u32");
    }
    let v = u32::from_le_bytes(data[..4].try_into().unwrap());
    *data = &data[4..];
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_roundtrip() {
        let mut d = Dictionary::new();
        let a = d.intern("HttpClient");
        let b = d.intern("Foo");
        assert_eq!(d.intern("HttpClient"), a);
        assert_ne!(a, b);
        let bytes = d.to_bytes();
        let d2 = Dictionary::from_bytes(&bytes).unwrap();
        assert_eq!(d2.resolve(a), Some("HttpClient"));
        assert_eq!(d2.lookup("Foo"), Some(b));
    }

    #[test]
    fn from_bytes_rejects_duplicate_strings() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        for s in ["Foo", "Foo"] {
            let b = s.as_bytes();
            bytes.extend_from_slice(&(b.len() as u32).to_le_bytes());
            bytes.extend_from_slice(b);
        }
        let err = Dictionary::from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn from_bytes_rejects_trailing_bytes() {
        let mut d = Dictionary::new();
        d.intern("Foo");
        let mut bytes = d.to_bytes();
        bytes.push(0xff);
        let err = Dictionary::from_bytes(&bytes).unwrap_err().to_string();
        assert!(err.contains("trailing"), "{err}");
    }
}
