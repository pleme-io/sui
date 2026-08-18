builtins.toXML { simple = x: x; pat = {a, b ? 1}: a; ell = {a, ...}: a; bound = args@{a}: a; both = args@{a, ...}: a; primop = builtins.add; partial = builtins.add 1; }
