//! Derivation builtins: derivation, derivationStrict, build_derivation.

use super::*;

pub(crate) fn register(builtins: &mut NixAttrs) {
    register_builtin(builtins, "derivation", |args| {
        build_derivation(&args[0])
    });

    // derivationStrict — alias to derivation
    register_builtin(builtins, "derivationStrict", |args| {
        build_derivation(&args[0])
    });
}
