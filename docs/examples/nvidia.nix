# NixOS recipe for the fans of an NVIDIA GPU. It goes with nvidia.toml.
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

    # The host has no IPMI device
    enableIpmi = false;

    # Run the service as root, which a GPU fan speed needs
    enableNvidiaFans = true;

    # The `nvidia` source of nvidia.toml runs nvidia-smi.
    # REPLACE: the driver package, when the host does not install the driver
    # through hardware.nvidia
    extraPackages = [(lib.getBin config.hardware.nvidia.package)];

    settings = builtins.fromTOML (builtins.readFile ./nvidia.toml);
  };
}
