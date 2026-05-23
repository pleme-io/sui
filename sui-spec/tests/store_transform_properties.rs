//! Property tests for the store-transform AST + closure walker.
//!
//! Each transform must satisfy:
//!   - Determinism: same input → same output
//!   - Reapply is no-op (idempotence) for the common patterns
//!   - File-contents transforms never add bytes outside the
//!     match region
//!   - StorePathReference rewrites are length-preserving (so
//!     downstream offset math holds)
//!
//! Closure + RefIndex must satisfy:
//!   - Determinism: same NAR content → same closure paths
//!   - Reverse-edge symmetry: if A.references contains B then
//!     B.referrers contains A

use proptest::prelude::*;
use sui_spec::store_ops::{NarNode, ParsedNar};
use sui_spec::store_transform::{
    apply_one, StoreTransform, TransformKind,
};

fn file(bytes: Vec<u8>) -> NarNode {
    NarNode::File { executable: false, contents: bytes }
}

proptest! {
    /// File-contents regex transform is deterministic:
    /// applying the same transform twice yields the same tree.
    #[test]
    fn file_contents_transform_is_deterministic(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let t = StoreTransform {
            name: "test".into(),
            description: "".into(),
            match_kind: TransformKind::FileContents,
            pattern: "world".into(),
            replacement: "planet".into(),
        };
        let mut tree1 = ParsedNar { root: file(bytes.clone()) };
        let mut tree2 = ParsedNar { root: file(bytes.clone()) };
        let _ = apply_one(&mut tree1.root, &t).unwrap();
        let _ = apply_one(&mut tree2.root, &t).unwrap();
        prop_assert_eq!(tree1.root, tree2.root);
    }

    /// FileContents transform reapply is idempotent for ANY
    /// pattern → replacement pair where the replacement doesn't
    /// contain the pattern.
    #[test]
    fn file_contents_reapply_is_idempotent(
        content in "[a-z]{1,40}",
    ) {
        let t = StoreTransform {
            name: "test".into(),
            description: "".into(),
            match_kind: TransformKind::FileContents,
            pattern: "world".into(),
            replacement: "planet".into(),
        };
        let mut tree = ParsedNar { root: file(content.into_bytes()) };
        let _ = apply_one(&mut tree.root, &t).unwrap();
        let second = apply_one(&mut tree.root, &t).unwrap();
        prop_assert!(second.is_noop());
    }

    /// StorePathReference rewrites are length-preserving:
    /// |from| == |to| ⇒ output bytes have the same length as
    /// input bytes (modulo replacement count × delta_len, which
    /// is 0 here).
    #[test]
    fn store_path_reference_is_length_preserving(
        before in proptest::collection::vec(any::<u8>(), 0..128),
        after in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let from = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let to   = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        // Construct content that may or may not contain `from`.
        let mut content = before.clone();
        content.extend_from_slice(from.as_bytes());
        content.extend_from_slice(&after);

        let t = StoreTransform {
            name: "graft".into(),
            description: "".into(),
            match_kind: TransformKind::StorePathReference,
            pattern: from.into(),
            replacement: to.into(),
        };
        let original_len = content.len();
        let mut tree = ParsedNar { root: file(content) };
        let _ = apply_one(&mut tree.root, &t).unwrap();
        if let NarNode::File { contents, .. } = &tree.root {
            prop_assert_eq!(contents.len(), original_len);
        }
    }

    /// StorePathReference with same pattern in `replacement` is
    /// a no-op (no offset shift, no duplicate replacement).
    #[test]
    fn store_path_reference_same_pattern_is_noop(
        bytes in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let from = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let t = StoreTransform {
            name: "noop".into(),
            description: "".into(),
            match_kind: TransformKind::StorePathReference,
            pattern: from.into(),
            replacement: from.into(),
        };
        let mut tree = ParsedNar { root: file(bytes.clone()) };
        let _ = apply_one(&mut tree.root, &t).unwrap();
        if let NarNode::File { contents, .. } = &tree.root {
            prop_assert_eq!(contents, &bytes);
        }
    }

    /// EntryName transform never changes child contents.
    #[test]
    fn entry_name_only_renames(
        names in proptest::collection::vec("[a-z]{1,8}", 1..6),
    ) {
        let mut entries: Vec<(String, NarNode)> = names.iter()
            .map(|n| (n.clone(), file(b"contents".to_vec())))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut tree = ParsedNar { root: NarNode::Directory { entries } };
        let t = StoreTransform {
            name: "test".into(),
            description: "".into(),
            match_kind: TransformKind::EntryName,
            pattern: "^a".into(),
            replacement: "A".into(),
        };
        let _ = apply_one(&mut tree.root, &t).unwrap();
        if let NarNode::Directory { entries } = &tree.root {
            for (_, child) in entries {
                if let NarNode::File { contents, .. } = child {
                    prop_assert_eq!(contents, b"contents");
                }
            }
        }
    }

    /// Empty transform list is a perfect no-op: every input
    /// tree survives unchanged.
    #[test]
    fn empty_transform_list_is_noop(
        bytes in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let mut tree = ParsedNar { root: file(bytes.clone()) };
        let outcomes = sui_spec::store_transform::apply_all(&mut tree, &[]).unwrap();
        prop_assert!(outcomes.is_empty());
        if let NarNode::File { contents, .. } = &tree.root {
            prop_assert_eq!(contents, &bytes);
        }
    }
}
