{
  description = "minimal sui flake fixture with no inputs";
  outputs = { self }: {
    answer = 42;
    greeting = "hello world";
    numbers = [ 1 2 3 ];
    nested = { a = 1; b = { c = 2; }; };
  };
}
