# NixOS recipe for a desktop or a server without IPMI. It goes with
# hwmon-desktop.toml.
#
# Copy both files into your host's configuration, change the values marked
# REPLACE in both, and import this file. docs/nixos.md shows how to add the
# coolerd flake to the host first.
{...}: {
  services.coolerd = {
    enable = true;

    # The host has no IPMI device
    enableIpmi = false;

    # REPLACE: the kernel module of the chip that drives your fans, when the
    # kernel does not load it by itself
    hwmon.kernelModules = ["nct6775"];

    # REPLACE: every chip and PWM channel that a `hwmon` fan in
    # hwmon-desktop.toml names. The service can write only these channels.
    hwmon.pwmChannels.nct6798 = [2];

    settings = builtins.fromTOML (builtins.readFile ./hwmon-desktop.toml);
  };
}
