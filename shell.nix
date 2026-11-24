# Compatibility shim for classic `nix-shell` users; the flake is the source
# of truth. `nix develop` (or direnv) does the same thing on a locked flake.
let
  flake = builtins.getFlake (toString ./.);
  system = builtins.currentSystem;
in
flake.devShells.${system}.default
