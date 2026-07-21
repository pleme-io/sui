# Directory-import target: `import ./dir` resolves here (default.nix rule).
{
  marker = "dir-default";
  # A relative path literal inside an imported file resolves against THIS
  # file's directory — the eval-dir mirror under test.
  origin = ./data.nix;
  data = import ./data.nix;
}
