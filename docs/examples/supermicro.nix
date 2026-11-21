# NixOS recipe for a Supermicro server. It goes with supermicro.toml.
#
# Copy both files into your host's configuration, change the values marked
# REPLACE in supermicro.toml, and import this file. docs/nixos.md shows how to
# add the coolerd flake to the host first.
{...}: {
  services.coolerd = {
    enable = true;

    # The `smart` sources of supermicro.toml run smartctl
    enableSmartMonTools = true;

    settings = builtins.fromTOML (builtins.readFile ./supermicro.toml);
  };
}
