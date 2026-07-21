# An attrset-module shape: a function from a config attrset to an attrset.
{ config }:
{
  enabled = config.enable or false;
  label = if config.enable or false then "on" else "off";
  paths = { self = ./module.nix; };
}
