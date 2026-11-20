# NixOS recipe for a Supermicro server with an NVIDIA GPU. It goes with
# mixed.toml.
#
# The host must already install the NVIDIA driver. Copy both files into your
# host's configuration, change the values marked REPLACE in both, and import
# this file. docs/nixos.md shows how to add the coolerd flake to the host
# first.
{
  config,
  lib,
  ...
}: {
  # coolerd loads libnvidia-ml.so.1 from /run/opengl-driver/lib, which exists
  # only when the host enables its graphics driver packages
  hardware.graphics.enable = true;

  services.coolerd = {
    enable = true;

    # Run the service as root, which a GPU fan speed needs. enableIpmi stays
    # on, its default, so the host loads the IPMI kernel modules.
    enableNvidiaFans = true;

    # The `nvidia` sources of mixed.toml run nvidia-smi.
    # REPLACE: the driver package, when the host does not install the driver
    # through hardware.nvidia
    extraPackages = [(lib.getBin config.hardware.nvidia.package)];

    settings = builtins.fromTOML (builtins.readFile ./mixed.toml);
  };
}
