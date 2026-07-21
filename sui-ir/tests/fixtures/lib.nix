# A let-heavy pure library file — the file-corpus "lib" fixture.
let
  double = x: x * 2;
  triple = x: x * 3;
  compose = f: g: x: f (g x);
  nums = [ 1 2 3 4 5 ];
  strings = { greeting = "hello"; subject = "world"; };
in
{
  inherit double triple compose nums strings;
  sextuple = compose double triple;
  sum = builtins.foldl' (a: b: a + b) 0 nums;
  greet = name: strings.greeting + " " + name;
  squares = builtins.genList (i: i * i) 6;
}
