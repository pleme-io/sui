# Half of a deliberate circular-import pair (with cycle_b.nix). The select
# forces the import during file evaluation, so the cycle is hit at import
# time. IR-only fixture: eval_ir_file reports a typed ImportCycle; the
# walker (like CppNix) would recurse until the stack dies.
(import ./cycle_b.nix).v
