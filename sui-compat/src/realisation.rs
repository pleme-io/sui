use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DrvOutput {
    pub drv_hash: String,
    pub output_name: String,
}

impl DrvOutput {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (hash, name) = s.split_once('!').ok_or_else(|| format!("Invalid derivation output id {s}"))?;
        let (algo, digest) = hash.split_once(':').ok_or_else(|| format!("Invalid derivation output id {s}"))?;
        let algo = crate::hash::HashAlgorithm::from_nix_str(algo).map_err(|e| e.to_string())?;
        let digest = crate::hash::NixHash::parse_any(algo, digest).map_err(|e| format!("{s}: {e}"))?;
        Ok(Self { drv_hash: format!("{algo}:{}", digest.to_hex()), output_name: name.to_string() })
    }
}

impl std::fmt::Display for DrvOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}!{}", self.drv_hash, self.output_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realisation {
    pub id: DrvOutput,
    pub out_path: String,
    pub signatures: BTreeSet<String>,
    pub dependent_realisations: BTreeMap<String, String>,
}

impl Realisation {
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut obj = BTreeMap::new();
        obj.insert("dependentRealisations", serde_json::json!(self.dependent_realisations));
        obj.insert("id", serde_json::json!(self.id.to_string()));
        obj.insert("outPath", serde_json::json!(self.out_path));
        obj.insert("signatures", serde_json::json!(self.signatures));
        serde_json::to_string(&obj).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        let field = |name: &str| {
            v.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("Drv output info file 'remote-protocol' is corrupt, missing field {name}"))
        };
        Ok(Self {
            id: DrvOutput::parse(&field("id")?)?,
            out_path: field("outPath")?,
            signatures: v
                .get("signatures")
                .and_then(serde_json::Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            dependent_realisations: v
                .get("dependentRealisations")
                .and_then(serde_json::Value::as_object)
                .map(|o| o.iter().filter_map(|(k, x)| x.as_str().map(|x| (k.clone(), x.to_string()))).collect())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_realisation_renders_as_cppnix_dumps_it_and_reads_back() {
        let r = Realisation {
            id: DrvOutput::parse("sha256:1b4sb93wp679q4zx9k1ignby1yna3z7c4c2ri3wphylbc2dwsys0!out").unwrap(),
            out_path: "0123456789abcdfghijklmnpqrsvwxyz-x".into(),
            signatures: BTreeSet::new(),
            dependent_realisations: BTreeMap::new(),
        };
        assert_eq!(r.id.drv_hash.len(), "sha256:".len() + 64);
        let json = r.to_json();
        assert_eq!(
            json,
            format!(
                r#"{{"dependentRealisations":{{}},"id":"{}!out","outPath":"0123456789abcdfghijklmnpqrsvwxyz-x","signatures":[]}}"#,
                r.id.drv_hash
            )
        );
        assert_eq!(Realisation::from_json(&json).unwrap(), r);
        assert!(DrvOutput::parse("nobang").is_err());
        assert!(Realisation::from_json("{}").is_err());
    }
}
