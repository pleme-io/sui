use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value as Json};

use crate::flake_lock::FlakeLockUpdateError as Error;
use crate::value::{NixAttrs, Value};

pub type Attrs = Map<String, Json>;
pub type InputPath = Vec<String>;

#[derive(Debug, Clone, Default)]
pub struct LockFlags {
    pub overrides: Vec<(InputPath, String)>,
    pub updates: BTreeSet<InputPath>,
    pub recreate: bool,
}

#[derive(Debug, Clone)]
pub struct Locked {
    pub path: PathBuf,
    pub json: String,
    pub existed: bool,
    pub changed: bool,
    pub diff: String,
    pub warnings: Vec<String>,
}

impl Locked {
    #[must_use]
    pub fn header(&self, verb_on_existing: &str) -> String {
        let path = self.path.display();
        if !self.existed {
            format!("creating lock file \"{path}\": \n{}", self.diff.trim_end())
        } else if self.diff.trim_end().is_empty() {
            format!("{verb_on_existing} lock file \"{path}\"")
        } else {
            format!("{verb_on_existing} lock file \"{path}\":\n{}", self.diff.trim_end())
        }
    }

    pub fn write(&self) -> Result<(), Error> {
        std::fs::write(&self.path, &self.json)?;
        Ok(())
    }
}

#[must_use]
pub fn parse_input_path(s: &str) -> InputPath {
    s.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

pub fn parse_flake_ref_attrs(url: &str) -> Result<Attrs, Error> {
    let value = crate::builtins::flake_parse::parse_flake_ref(url).map_err(|e| Error::FlakeParse(e.to_string()))?;
    nix_attrs_to_json(&value.to_attrs().map_err(|e| Error::FlakeParse(e.to_string()))?)
}

pub fn lock_flake(flake_dir: &Path, flags: &LockFlags) -> Result<Locked, Error> {
    let lock_path = flake_dir.join("flake.lock");
    let old_text = std::fs::read_to_string(&lock_path).ok();
    let old_doc = match &old_text {
        Some(text) => Some(parse_lock_doc(text)?),
        None => None,
    };
    let inputs = read_flake_inputs(flake_dir, &[])?;
    let mut locker = Locker::new(flags);
    for (path, url) in &flags.overrides {
        locker.overrides.insert(
            path.clone(),
            FlakeInput { reference: Some(parse_flake_ref_attrs(url)?), ..FlakeInput::default() },
        );
        locker.cli.insert(path.clone());
    }
    let mut root = Node::default();
    let old_root = if flags.recreate { None } else { old_doc.as_ref().map(OldRef::root) };
    locker.compute(&inputs, &mut root, &[], old_root.as_ref(), &[], false)?;
    for (path, url) in &flags.overrides {
        if !locker.used_overrides.contains(path) {
            locker.warnings.push(format!("the flag '--override-input {} {url}' does not match any input", path.join("/")));
        }
    }
    for path in &flags.updates {
        if !locker.used_updates.contains(path) {
            locker.warnings.push(format!("'{}' does not match any input of this flake", path.join("/")));
        }
    }
    check_follows(&root)?;
    let new_doc = to_lock_json(&root);
    let lock = sui_compat::flake::FlakeLock::parse(&new_doc.to_string()).map_err(|e| Error::FlakeParse(e.to_string()))?;
    let json = lock.to_json().map_err(|e| Error::FlakeParse(e.to_string()))?;
    let old_flat = old_doc.as_ref().map(|d| flatten_old(&OldRef::root(d))).unwrap_or_default();
    let changed = old_text
        .as_deref()
        .and_then(|t| serde_json::from_str::<Json>(t).ok())
        .is_none_or(|old| old != new_doc);
    Ok(Locked {
        path: lock_path,
        json,
        existed: old_text.is_some(),
        changed,
        diff: diff(&old_flat, &flatten_new(&root)),
        warnings: locker.warnings,
    })
}

#[derive(Debug, Clone)]
struct FlakeInput {
    reference: Option<Attrs>,
    follows: Option<InputPath>,
    is_flake: bool,
    overrides: BTreeMap<String, FlakeInput>,
}

impl Default for FlakeInput {
    fn default() -> Self {
        Self { reference: None, follows: None, is_flake: true, overrides: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Default)]
struct Node {
    inputs: BTreeMap<String, Edge>,
    locked: Option<Attrs>,
    original: Option<Attrs>,
    is_flake: bool,
    parent: Option<InputPath>,
}

#[derive(Debug, Clone)]
enum Edge {
    Node(Box<Node>),
    Follows(InputPath),
}

#[derive(Clone)]
struct OldRef {
    nodes: Rc<Attrs>,
    key: String,
}

enum OldEdge {
    Node(OldRef),
    Follows(InputPath),
}

impl OldRef {
    fn root(doc: &(Rc<Attrs>, String)) -> Self {
        Self { nodes: doc.0.clone(), key: doc.1.clone() }
    }

    fn json(&self) -> Option<&Attrs> {
        self.nodes.get(&self.key).and_then(Json::as_object)
    }

    fn field(&self, name: &str) -> Option<Attrs> {
        self.json().and_then(|n| n.get(name)).and_then(Json::as_object).cloned()
    }

    fn is_flake(&self) -> bool {
        self.json().and_then(|n| n.get("flake")).and_then(Json::as_bool).unwrap_or(true)
    }

    fn parent(&self) -> Option<InputPath> {
        self.json().and_then(|n| n.get("parent")).and_then(json_path)
    }

    fn inputs(&self) -> BTreeMap<String, OldEdge> {
        let mut out = BTreeMap::new();
        let Some(inputs) = self.json().and_then(|n| n.get("inputs")).and_then(Json::as_object) else { return out };
        for (id, edge) in inputs {
            match edge {
                Json::String(key) if self.nodes.contains_key(key) => {
                    out.insert(id.clone(), OldEdge::Node(Self { nodes: self.nodes.clone(), key: key.clone() }));
                }
                Json::Array(_) => {
                    out.insert(id.clone(), OldEdge::Follows(json_path(edge).unwrap_or_default()));
                }
                _ => {}
            }
        }
        out
    }
}

fn json_path(v: &Json) -> Option<InputPath> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
}

fn parse_lock_doc(text: &str) -> Result<(Rc<Attrs>, String), Error> {
    let doc: Json = serde_json::from_str(text)?;
    let nodes = doc.get("nodes").and_then(Json::as_object).cloned().ok_or(Error::InvalidFormat)?;
    let root = doc.get("root").and_then(Json::as_str).unwrap_or("root").to_string();
    Ok((Rc::new(nodes), root))
}

struct Locker<'a> {
    flags: &'a LockFlags,
    overrides: BTreeMap<InputPath, FlakeInput>,
    cli: BTreeSet<InputPath>,
    used_overrides: BTreeSet<InputPath>,
    used_updates: BTreeSet<InputPath>,
    parents: Vec<Attrs>,
    warnings: Vec<String>,
    fetcher: crate::fetcher::InputFetcher,
}

impl<'a> Locker<'a> {
    fn new(flags: &'a LockFlags) -> Self {
        Self {
            flags,
            overrides: BTreeMap::new(),
            cli: BTreeSet::new(),
            used_overrides: BTreeSet::new(),
            used_updates: BTreeSet::new(),
            parents: Vec::new(),
            warnings: Vec::new(),
            fetcher: crate::fetcher::InputFetcher::new(),
        }
    }

    fn add_overrides(&mut self, input: &FlakeInput, path: &[String]) {
        for (id, over) in &input.overrides {
            let child = [path, std::slice::from_ref(id)].concat();
            if over.reference.is_some() || over.follows.is_some() {
                self.overrides
                    .entry(child.clone())
                    .or_insert_with(|| over.clone());
            }
            self.add_overrides(over, &child);
        }
    }

    fn compute(
        &mut self,
        inputs: &BTreeMap<String, FlakeInput>,
        node: &mut Node,
        prefix: &[String],
        old: Option<&OldRef>,
        follows_prefix: &[String],
        trust_lock: bool,
    ) -> Result<(), Error> {
        for (id, input) in inputs {
            let path = [prefix, std::slice::from_ref(id)].concat();
            self.add_overrides(input, &path);
        }
        for path in self.overrides.keys() {
            if let Some((last, parent)) = path.split_last()
                && parent == prefix
                && !inputs.contains_key(last)
            {
                self.warnings.push(format!("input '{}' has an override for a non-existent input '{last}'", prefix.join("/")));
            }
        }
        let old_inputs = old.map(OldRef::inputs).unwrap_or_default();
        for (id, declared) in inputs {
            let path = [prefix, std::slice::from_ref(id)].concat();
            let printed = path.join("/");
            let has_cli = self.cli.contains(&path);
            let mut input = declared.clone();
            if let Some(o) = self.overrides.get(&path) {
                input = o.clone();
                input.is_flake = declared.is_flake;
                self.used_overrides.insert(path.clone());
            }
            if let Some(follows) = input.follows.clone() {
                node.inputs.insert(id.clone(), Edge::Follows(follows));
                continue;
            }
            let reference = input.reference.clone().unwrap_or_else(|| {
                Attrs::from_iter([("id".to_string(), Json::from(id.as_str())), ("type".to_string(), Json::from("indirect"))])
            });
            if reference.get("type").and_then(Json::as_str) == Some("path")
                && !reference.get("path").and_then(Json::as_str).is_some_and(|p| p.starts_with('/'))
            {
                return Err(Error::UnsupportedType(format!(
                    "flake input '{printed}' is a relative path input, which sui does not lock yet"
                )));
            }
            self.used_updates.insert(path.clone());
            let old_lock = if self.flags.updates.contains(&path) {
                None
            } else {
                match old_inputs.get(id) {
                    Some(OldEdge::Node(r)) => Some(r.clone()),
                    _ => None,
                }
            };
            let reusable = old_lock.as_ref().filter(|ol| {
                ol.field("original").is_some_and(|o| o == reference) && ol.parent().is_none() && !has_cli
            });
            if let Some(ol) = reusable {
                let mut child = Node {
                    inputs: BTreeMap::new(),
                    locked: ol.field("locked"),
                    original: ol.field("original"),
                    is_flake: ol.is_flake(),
                    parent: ol.parent(),
                };
                let mut must_refetch = self
                    .flags
                    .updates
                    .range(path.clone()..)
                    .next()
                    .is_some_and(|lb| lb.len() > path.len() && lb.starts_with(&path));
                let mut fake = BTreeMap::new();
                if !must_refetch {
                    for (i, edge) in ol.inputs() {
                        match edge {
                            OldEdge::Node(r) => {
                                fake.insert(i, FlakeInput { reference: r.field("original"), is_flake: r.is_flake(), ..FlakeInput::default() });
                            }
                            OldEdge::Follows(f) => {
                                let over = [path.as_slice(), std::slice::from_ref(&i)].concat();
                                if !trust_lock && !self.overrides.contains_key(&over) {
                                    must_refetch = true;
                                    break;
                                }
                                fake.insert(i, FlakeInput { follows: Some([follows_prefix, f.as_slice()].concat()), ..FlakeInput::default() });
                            }
                        }
                    }
                }
                if must_refetch {
                    let locked = child.locked.clone().ok_or(Error::InvalidFormat)?;
                    let dir = self.fetch_locked(&locked, &printed)?;
                    let flake_inputs = read_flake_inputs(&dir, &path)?;
                    self.compute(&flake_inputs, &mut child, &path, Some(ol), follows_prefix, false)?;
                } else {
                    self.compute(&fake, &mut child, &path, Some(ol), follows_prefix, true)?;
                }
                node.inputs.insert(id.clone(), Edge::Node(Box::new(child)));
                continue;
            }
            let original = match (&declared.reference, has_cli) {
                (Some(r), true) => r.clone(),
                _ => reference.clone(),
            };
            let (locked, dir) = self.fetch(&reference, &printed)?;
            let mut child = Node { inputs: BTreeMap::new(), locked: Some(locked), original: Some(original), is_flake: input.is_flake, parent: None };
            if input.is_flake {
                if self.parents.contains(&reference) {
                    return Err(Error::FlakeParse(format!("found circular import of flake '{}'", ref_to_string(&reference))));
                }
                self.parents.push(reference.clone());
                let flake_inputs = read_flake_inputs(&dir, &path)?;
                let sub_lock = match std::fs::read_to_string(dir.join("flake.lock")) {
                    Ok(text) => Some(OldRef::root(&parse_lock_doc(&text)?)),
                    Err(_) => None,
                };
                let result = self.compute(&flake_inputs, &mut child, &path, sub_lock.as_ref(), &path, false);
                self.parents.pop();
                result?;
            }
            node.inputs.insert(id.clone(), Edge::Node(Box::new(child)));
        }
        Ok(())
    }

    fn fetch(&mut self, reference: &Attrs, printed: &str) -> Result<(Attrs, PathBuf), Error> {
        let ty = reference.get("type").and_then(Json::as_str).unwrap_or_default();
        match ty {
            "indirect" => {
                let resolved = crate::builtins::flake_registry::resolve_indirect(&json_to_nix_attrs(reference))
                    .and_then(|v| v.to_attrs())
                    .map_err(|e| Error::FetchFailed(format!("flake input '{printed}': {e}")))?;
                let resolved = nix_attrs_to_json(&resolved)?;
                if resolved.get("type").and_then(Json::as_str) == Some("indirect") {
                    return Err(Error::FetchFailed(format!("flake input '{printed}': registry resolved to another registry name")));
                }
                self.fetch(&resolved, printed)
            }
            "github" => {
                let original: sui_compat::flake::OriginalInput = serde_json::from_value(Json::Object(reference.clone()))?;
                let locked = crate::flake_lock::resolve_github(&original)
                    .map_err(|e| Error::FetchFailed(format!("flake input '{printed}': {e}")))?;
                let attrs = match serde_json::to_value(&locked)? {
                    Json::Object(m) => m,
                    _ => return Err(Error::InvalidFormat),
                };
                let dir = self.fetch_locked(&attrs, printed)?;
                Ok((attrs, dir))
            }
            "path" => {
                let raw = reference.get("path").and_then(Json::as_str).unwrap_or_default();
                let dir = PathBuf::from(raw);
                let source = sui_compat::source::nar_hash_source_tree(&dir, "source")
                    .map_err(|e| Error::FetchFailed(format!("flake input '{printed}': {}: {e}", dir.display())))?;
                let mut attrs = reference.clone();
                attrs.remove("narHash");
                attrs.insert("narHash".into(), Json::from(source.nar_hash_sri));
                if !attrs.contains_key("lastModified") {
                    attrs.insert("lastModified".into(), Json::from(max_mtime(&dir)));
                }
                Ok((attrs, subdir(&dir, reference)))
            }
            other => Err(Error::UnsupportedType(format!(
                "flake input '{printed}' has type '{other}', which sui does not lock yet (github, path and registry inputs are)"
            ))),
        }
    }

    fn fetch_locked(&mut self, locked: &Attrs, printed: &str) -> Result<PathBuf, Error> {
        if locked.get("type").and_then(Json::as_str) == Some("path") {
            let raw = locked.get("path").and_then(Json::as_str).unwrap_or_default();
            return Ok(subdir(Path::new(raw), locked));
        }
        let typed: sui_compat::flake::LockedInput = serde_json::from_value(Json::Object(locked.clone()))?;
        let mut key = typed.clone();
        key.dir = None;
        let base = self
            .fetcher
            .fetch(&key)
            .map_err(|e| Error::FetchFailed(format!("flake input '{printed}': {e}")))?;
        Ok(subdir(&base, locked))
    }
}

fn subdir(base: &Path, attrs: &Attrs) -> PathBuf {
    match attrs.get("dir").and_then(Json::as_str) {
        Some(d) if !d.is_empty() => base.join(d),
        _ => base.to_path_buf(),
    }
}

fn max_mtime(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    let own = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    if !meta.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| max_mtime(&e.path()))
        .fold(own, u64::max)
}

fn read_flake_inputs(dir: &Path, lock_root: &[String]) -> Result<BTreeMap<String, FlakeInput>, Error> {
    let flake_nix = dir.join("flake.nix");
    let source = std::fs::read_to_string(&flake_nix)
        .map_err(|e| Error::FlakeParse(format!("{}: {e}", flake_nix.display())))?;
    let flake = crate::eval::eval_with_file(&source, Some(flake_nix.clone()))
        .and_then(|v| v.to_attrs())
        .map_err(|e| Error::FlakeParse(format!("{}: {e}", flake_nix.display())))?;
    let Some(inputs) = flake.get("inputs") else { return Ok(BTreeMap::new()) };
    let inputs = force_attrs(inputs, &flake_nix)?;
    let mut out = BTreeMap::new();
    for (name, value) in inputs.iter() {
        if name == "self" {
            continue;
        }
        out.insert(name.clone(), parse_input(value, lock_root, &flake_nix, dir)?);
    }
    Ok(out)
}

fn force_attrs(value: &Value, file: &Path) -> Result<NixAttrs, Error> {
    crate::eval::force_value(value)
        .and_then(|v| v.to_attrs())
        .map_err(|e| Error::FlakeParse(format!("{}: {e}", file.display())))
}

fn parse_input(value: &Value, lock_root: &[String], file: &Path, dir: &Path) -> Result<FlakeInput, Error> {
    let attrs = force_attrs(value, file)?;
    let mut input = FlakeInput::default();
    let mut ref_attrs = Attrs::new();
    let mut url = None;
    let fail = |e: crate::value::EvalError| Error::FlakeParse(format!("{}: {e}", file.display()));
    for (name, v) in attrs.iter() {
        let v = crate::eval::force_value(v).map_err(fail)?;
        match name.as_str() {
            "url" => {
                let s = match &v {
                    Value::Path(p) => {
                        let p = p.as_str();
                        let rel = Path::new(p).strip_prefix(dir).map_or_else(|_| p.to_string(), |r| format!("./{}", r.display()));
                        format!("path:{rel}")
                    }
                    other => other.as_string().map_err(fail)?.to_string(),
                };
                ref_attrs.insert("url".into(), Json::from(s.clone()));
                url = Some(s);
            }
            "flake" => input.is_flake = v.as_bool().map_err(fail)?,
            "inputs" => {
                let nested = v.to_attrs().map_err(fail)?;
                for (k, nv) in nested.iter() {
                    input.overrides.insert(k.clone(), parse_input(nv, lock_root, file, dir)?);
                }
            }
            "follows" => {
                let f = parse_input_path(v.as_string().map_err(fail)?);
                input.follows = Some([lock_root, f.as_slice()].concat());
            }
            _ => {
                ref_attrs.insert(name.clone(), scalar_json(&v).ok_or_else(|| {
                    Error::FlakeParse(format!("{}: flake input attribute '{name}' is a {}", file.display(), v.type_name()))
                })?);
            }
        }
    }
    if ref_attrs.contains_key("type") {
        input.reference = Some(ref_attrs);
    } else {
        ref_attrs.remove("url");
        if let Some(name) = ref_attrs.keys().next() {
            return Err(Error::FlakeParse(format!("unexpected flake input attribute '{name}', at {}", file.display())));
        }
        if let Some(url) = url {
            input.reference = Some(parse_flake_ref_attrs(&url)?);
        }
    }
    if input.reference.is_some() && input.follows.is_some() {
        return Err(Error::FlakeParse(format!("flake input has both a flake reference and a follows attribute, at {}", file.display())));
    }
    Ok(input)
}

fn scalar_json(v: &Value) -> Option<Json> {
    match v {
        Value::Bool(b) => Some(Json::from(*b)),
        Value::Int(i) => Some(Json::from(*i)),
        Value::String(_) => v.as_string().ok().map(Json::from),
        _ => None,
    }
}

fn nix_attrs_to_json(attrs: &NixAttrs) -> Result<Attrs, Error> {
    let mut out = Attrs::new();
    for (k, v) in attrs.iter() {
        let v = crate::eval::force_value(v).map_err(|e| Error::FlakeParse(e.to_string()))?;
        if let Some(j) = scalar_json(&v) {
            out.insert(k.clone(), j);
        }
    }
    Ok(out)
}

fn json_to_nix_attrs(attrs: &Attrs) -> NixAttrs {
    let mut out = NixAttrs::new();
    for (k, v) in attrs {
        let value = match v {
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Int(n.as_i64().unwrap_or_default()),
            Json::String(s) => Value::string(s.clone()),
            _ => continue,
        };
        out.insert(k.clone(), value);
    }
    out
}

fn find_input<'n>(root: &'n Node, path: &[String], depth: usize) -> Option<&'n Node> {
    if depth > 64 {
        return None;
    }
    let mut node = root;
    for id in path {
        node = match node.inputs.get(id)? {
            Edge::Node(n) => n,
            Edge::Follows(f) => find_input(root, f, depth + 1)?,
        };
    }
    Some(node)
}

fn check_follows(root: &Node) -> Result<(), Error> {
    for (path, edge) in flatten_new(root) {
        if let FlatEdge::Follows(f) = edge
            && !f.is_empty()
            && find_input(root, &f, 0).is_none()
        {
            return Err(Error::FlakeParse(format!(
                "input '{}' follows a non-existent input '{}'",
                path.join("/"),
                f.join("/")
            )));
        }
    }
    Ok(())
}

fn to_lock_json(root: &Node) -> Json {
    fn dump(key: &str, node: &Node, keys: &mut BTreeSet<String>, nodes: &mut Attrs) -> String {
        let mut key = key.to_string();
        if !keys.insert(key.clone()) {
            key = (2..).map(|n| format!("{key}_{n}")).find(|k| keys.insert(k.clone())).unwrap_or_default();
        }
        let mut n = Attrs::new();
        if !node.inputs.is_empty() {
            let mut inputs = Attrs::new();
            for (id, edge) in &node.inputs {
                let v = match edge {
                    Edge::Node(child) => Json::from(dump(id, child, keys, nodes)),
                    Edge::Follows(f) => Json::from(f.clone()),
                };
                inputs.insert(id.clone(), v);
            }
            n.insert("inputs".into(), Json::Object(inputs));
        }
        if let (Some(locked), Some(original)) = (&node.locked, &node.original) {
            n.insert("locked".into(), Json::Object(locked.clone()));
            n.insert("original".into(), Json::Object(original.clone()));
            if !node.is_flake {
                n.insert("flake".into(), Json::from(false));
            }
            if let Some(p) = &node.parent {
                n.insert("parent".into(), Json::from(p.clone()));
            }
        }
        nodes.insert(key.clone(), Json::Object(n));
        key
    }
    let mut nodes = Attrs::new();
    let root_key = dump("root", root, &mut BTreeSet::new(), &mut nodes);
    serde_json::json!({ "nodes": nodes, "root": root_key, "version": 7 })
}

#[derive(Debug, Clone, PartialEq)]
enum FlatEdge {
    Locked(Attrs),
    Follows(InputPath),
}

fn flatten_new(root: &Node) -> BTreeMap<InputPath, FlatEdge> {
    fn walk(node: &Node, prefix: &[String], out: &mut BTreeMap<InputPath, FlatEdge>) {
        for (id, edge) in &node.inputs {
            let path = [prefix, std::slice::from_ref(id)].concat();
            match edge {
                Edge::Node(child) => {
                    out.entry(path.clone()).or_insert_with(|| FlatEdge::Locked(child.locked.clone().unwrap_or_default()));
                    walk(child, &path, out);
                }
                Edge::Follows(f) => {
                    out.entry(path).or_insert_with(|| FlatEdge::Follows(f.clone()));
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &[], &mut out);
    out
}

fn flatten_old(root: &OldRef) -> BTreeMap<InputPath, FlatEdge> {
    fn walk(node: &OldRef, prefix: &[String], done: &mut BTreeSet<String>, out: &mut BTreeMap<InputPath, FlatEdge>) {
        if !done.insert(node.key.clone()) {
            return;
        }
        for (id, edge) in node.inputs() {
            let path = [prefix, std::slice::from_ref(&id)].concat();
            match edge {
                OldEdge::Node(child) => {
                    out.entry(path.clone()).or_insert_with(|| FlatEdge::Locked(child.field("locked").unwrap_or_default()));
                    walk(&child, &path, done, out);
                }
                OldEdge::Follows(f) => {
                    out.entry(path).or_insert(FlatEdge::Follows(f));
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &[], &mut BTreeSet::new(), &mut out);
    out
}

fn describe(edge: &FlatEdge) -> String {
    match edge {
        FlatEdge::Follows(f) => format!("follows '{}'", f.join("/")),
        FlatEdge::Locked(attrs) => {
            let mut s = format!("'{}'", ref_to_string(attrs));
            if let Some(t) = attrs.get("lastModified").and_then(Json::as_i64) {
                let d = crate::builtins::fetchers::format_unix_yyyymmddhhmmss(t);
                s.push_str(&format!(" ({}-{}-{})", &d[0..4], &d[4..6], &d[6..8]));
            }
            s
        }
    }
}

fn diff(old: &BTreeMap<InputPath, FlatEdge>, new: &BTreeMap<InputPath, FlatEdge>) -> String {
    let mut out = String::new();
    let keys: BTreeSet<&InputPath> = old.keys().chain(new.keys()).collect();
    for path in keys {
        let name = path.join("/");
        match (old.get(path), new.get(path)) {
            (None, Some(n)) => out.push_str(&format!("• Added input '{name}':\n    {}\n", describe(n))),
            (Some(_), None) => out.push_str(&format!("• Removed input '{name}'\n")),
            (Some(o), Some(n)) if o != n => {
                out.push_str(&format!("• Updated input '{name}':\n    {}\n  → {}\n", describe(o), describe(n)));
            }
            _ => {}
        }
    }
    out
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~:@/?".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn query(attrs: &Attrs, skip: &[&str]) -> String {
    let sorted: BTreeMap<&String, &Json> = attrs.iter().collect();
    let parts: Vec<String> = sorted
        .into_iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .map(|(k, v)| {
            let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            format!("{k}={}", percent_encode(&v))
        })
        .collect();
    if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
}

fn ref_to_string(attrs: &Attrs) -> String {
    let get = |k: &str| attrs.get(k).and_then(Json::as_str).unwrap_or_default().to_string();
    let ty = get("type");
    match ty.as_str() {
        "github" | "gitlab" | "sourcehut" => {
            let mut s = format!("{ty}:{}/{}", get("owner"), get("repo"));
            let rev_or_ref = if attrs.contains_key("rev") { get("rev") } else { get("ref") };
            if !rev_or_ref.is_empty() {
                s.push('/');
                s.push_str(&rev_or_ref);
            }
            s + &query(attrs, &["type", "owner", "repo", "rev", "ref", "lastModified"])
        }
        "path" => format!("path:{}", get("path")) + &query(attrs, &["type", "path"]),
        "indirect" => {
            let mut s = format!("flake:{}", get("id"));
            for k in ["ref", "rev"] {
                if attrs.contains_key(k) {
                    s.push('/');
                    s.push_str(&get(k));
                }
            }
            s + &query(attrs, &["type", "id", "ref", "rev"])
        }
        "git" => format!("git+{}", get("url")) + &query(attrs, &["type", "url", "lastModified", "narHash", "revCount"]),
        "tarball" | "file" => {
            let url = get("url");
            let extra = query(attrs, &["type", "url", "lastModified", "rev", "revCount"]);
            match extra.strip_prefix('?') {
                Some(q) if url.contains('?') => format!("{url}&{q}"),
                _ => url + &extra,
            }
        }
        _ => format!("{ty}:{}", get("url")) + &query(attrs, &["type", "url"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_flake(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("flake.nix"), body).unwrap();
    }

    #[test]
    fn a_flake_with_no_lock_file_gets_one_with_follows_and_a_non_flake_input() {
        let tmp = tempfile::tempdir().unwrap();
        let dep = tmp.path().join("dep");
        let data = tmp.path().join("data");
        path_flake(&dep, r#"{ inputs.nixpkgs.url = "path:/nonexistent-unused"; outputs = _: {}; }"#);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("x"), "x").unwrap();
        let root = tmp.path().join("root");
        path_flake(
            &root,
            &format!(
                r#"{{
                  inputs.dep.url = "path:{}";
                  inputs.dep.inputs.nixpkgs.follows = "data";
                  inputs.data = {{ url = "path:{}"; flake = false; }};
                  outputs = _: {{}};
                }}"#,
                dep.display(),
                data.display()
            ),
        );
        let locked = lock_flake(&root, &LockFlags::default()).unwrap();
        assert!(!locked.existed && locked.changed);
        let doc: Json = serde_json::from_str(&locked.json).unwrap();
        assert_eq!(doc["root"], "root");
        assert_eq!(doc["version"], 7);
        assert_eq!(doc["nodes"]["root"]["inputs"], serde_json::json!({"data": "data", "dep": "dep"}));
        assert_eq!(doc["nodes"]["dep"]["inputs"], serde_json::json!({"nixpkgs": ["data"]}));
        assert_eq!(doc["nodes"]["data"]["flake"], false);
        assert_eq!(doc["nodes"]["data"]["original"], serde_json::json!({"path": data.display().to_string(), "type": "path"}));
        let nar = doc["nodes"]["data"]["locked"]["narHash"].as_str().unwrap();
        assert_eq!(nar, sui_compat::source::nar_hash_source_tree(&data, "source").unwrap().nar_hash_sri);
        assert!(locked.diff.contains("• Added input 'dep/nixpkgs':\n    follows 'data'\n"), "{}", locked.diff);
        assert!(locked.json.ends_with("}\n"));
    }

    #[test]
    fn an_unchanged_lock_is_kept_and_a_sub_flake_lock_supplies_its_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("leaf");
        path_flake(&leaf, "{ outputs = _: {}; }");
        let dep = tmp.path().join("dep");
        path_flake(&dep, &format!(r#"{{ inputs.leaf.url = "path:{}"; outputs = _: {{}}; }}"#, leaf.display()));
        let pinned = serde_json::json!({"lastModified": 1, "narHash": "sha256-pinned", "path": leaf.display().to_string(), "type": "path"});
        let dep_lock = serde_json::json!({
            "nodes": {
                "leaf": {"locked": pinned, "original": {"path": leaf.display().to_string(), "type": "path"}},
                "root": {"inputs": {"leaf": "leaf"}}
            },
            "root": "root", "version": 7
        });
        std::fs::write(dep.join("flake.lock"), dep_lock.to_string()).unwrap();
        let root = tmp.path().join("root");
        path_flake(&root, &format!(r#"{{ inputs.dep.url = "path:{}"; outputs = _: {{}}; }}"#, dep.display()));
        let first = lock_flake(&root, &LockFlags::default()).unwrap();
        let doc: Json = serde_json::from_str(&first.json).unwrap();
        assert_eq!(doc["nodes"]["leaf"]["locked"]["narHash"], "sha256-pinned");
        first.write().unwrap();
        let second = lock_flake(&root, &LockFlags::default()).unwrap();
        assert!(second.existed && !second.changed && second.diff.is_empty());
        assert_eq!(second.json, first.json);
    }

    #[test]
    fn an_override_input_replaces_the_locked_node_and_keeps_the_declared_original() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        path_flake(&a, "{ outputs = _: { v = 1; }; }");
        path_flake(&b, "{ outputs = _: { v = 2; }; }");
        let root = tmp.path().join("root");
        path_flake(&root, &format!(r#"{{ inputs.x.url = "path:{}"; outputs = _: {{}}; }}"#, a.display()));
        lock_flake(&root, &LockFlags::default()).unwrap().write().unwrap();
        let flags = LockFlags { overrides: vec![(vec!["x".into()], format!("path:{}", b.display()))], ..LockFlags::default() };
        let over = lock_flake(&root, &flags).unwrap();
        let doc: Json = serde_json::from_str(&over.json).unwrap();
        assert_eq!(doc["nodes"]["x"]["locked"]["path"], b.display().to_string());
        assert_eq!(doc["nodes"]["x"]["original"]["path"], a.display().to_string());
        assert!(over.changed && over.diff.starts_with("• Updated input 'x':"), "{}", over.diff);
    }

    #[test]
    fn a_follows_to_a_missing_input_is_refused_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        path_flake(&a, "{ outputs = _: {}; }");
        let root = tmp.path().join("root");
        path_flake(&root, &format!(r#"{{ inputs.x.url = "path:{}"; inputs.y.follows = "nope"; outputs = _: {{}}; }}"#, a.display()));
        let err = lock_flake(&root, &LockFlags::default()).unwrap_err().to_string();
        assert!(err.contains("input 'y' follows a non-existent input 'nope'"), "{err}");
    }

    #[test]
    fn colliding_node_names_take_cppnix_numeric_suffixes() {
        let mut root = Node::default();
        let leaf = |p: &str| {
            Edge::Node(Box::new(Node {
                locked: Some(Attrs::from_iter([("type".to_string(), Json::from("path")), ("path".to_string(), Json::from(p))])),
                original: Some(Attrs::from_iter([("type".to_string(), Json::from("path")), ("path".to_string(), Json::from(p))])),
                is_flake: true,
                ..Node::default()
            }))
        };
        let mut a = match leaf("/a") { Edge::Node(n) => *n, Edge::Follows(_) => unreachable!() };
        a.inputs.insert("nixpkgs".into(), leaf("/n1"));
        root.inputs.insert("a".into(), Edge::Node(Box::new(a)));
        root.inputs.insert("nixpkgs".into(), leaf("/n2"));
        let doc = to_lock_json(&root);
        assert_eq!(doc["nodes"]["a"]["inputs"]["nixpkgs"], "nixpkgs");
        assert_eq!(doc["nodes"]["root"]["inputs"]["nixpkgs"], "nixpkgs_2");
        assert_eq!(doc["nodes"]["nixpkgs_2"]["locked"]["path"], "/n2");
    }

    #[test]
    fn locked_tarball_and_git_refs_print_as_cppnix_describes_them() {
        let tarball: Attrs = serde_json::from_value(serde_json::json!({
            "lastModified": 1733328505, "narHash": "sha256-NeCCThCEP3eCl2l/+27kNNK7QrwZB1IJCrXfrbv5oqU=",
            "rev": "ff81ac966bb2cae68946d5ed5fc4994f96d0ffec", "revCount": 69, "type": "tarball",
            "url": "https://api.flakehub.com/f/pinned/edolstra/flake-compat/1.1.0/01948eb7-9cba-704f-bbf3-3fa956735b52/source.tar.gz"
        }))
        .unwrap();
        assert_eq!(
            describe(&FlatEdge::Locked(tarball)),
            "'https://api.flakehub.com/f/pinned/edolstra/flake-compat/1.1.0/01948eb7-9cba-704f-bbf3-3fa956735b52/source.tar.gz?narHash=sha256-NeCCThCEP3eCl2l/%2B27kNNK7QrwZB1IJCrXfrbv5oqU%3D' (2024-12-04)"
        );
        let git: Attrs = serde_json::from_value(serde_json::json!({
            "lastModified": 1777234348, "narHash": "sha256-fKw44a4qbUuI5eTG8k0gPbqMV5TOrjYF35PBzsYgd2U=",
            "ref": "refs/heads/main", "rev": "2c781c0609ecda600ab98f98cca417bbd981bd53", "revCount": 1677,
            "type": "git", "url": "https://codeberg.org/jcollie/zig-overlay.git"
        }))
        .unwrap();
        assert_eq!(
            describe(&FlatEdge::Locked(git)),
            "'git+https://codeberg.org/jcollie/zig-overlay.git?ref=refs/heads/main&rev=2c781c0609ecda600ab98f98cca417bbd981bd53' (2026-04-26)"
        );
    }

    #[test]
    fn a_locked_github_ref_prints_as_cppnix_describes_it() {
        let attrs: Attrs = serde_json::from_value(serde_json::json!({
            "lastModified": 1777777777, "narHash": "sha256-Lqv9MZO0XAGcMbXJU+ULBSMD41Pf391uJehylUQKe7Y=",
            "owner": "o", "repo": "r", "rev": "dbba5f888c82ef3ce594c451c33ac2474eb80847", "type": "github"
        }))
        .unwrap();
        assert_eq!(
            describe(&FlatEdge::Locked(attrs)),
            "'github:o/r/dbba5f888c82ef3ce594c451c33ac2474eb80847?narHash=sha256-Lqv9MZO0XAGcMbXJU%2BULBSMD41Pf391uJehylUQKe7Y%3D' (2026-05-03)"
        );
    }
}
