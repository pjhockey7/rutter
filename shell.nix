# `nix-shell` entrypoint; the real definition lives in ./default.nix.
{ system ? builtins.currentSystem }:
(import ./default.nix { inherit system; }).shells.default
