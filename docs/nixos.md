# NixOS

This repository is a Nix flake. It provides a NixOS module, `nixosModules.default`, and an overlay, `overlays.default`, that adds the `coolerd` package. This page shows how to add both to a host, describes every option of the module, and covers the move from `services.ipmi-fan-control`.

- [Adding coolerd to a host](#adding-coolerd-to-a-host)
- [Recipes](#recipes)
- [What the module sets up](#what-the-module-sets-up)
- [Options](#options)
- [The hwmon access helper](#the-hwmon-access-helper)
- [NVIDIA fans and root](#nvidia-fans-and-root)
- [Checking NVIDIA fans on a host](#checking-nvidia-fans-on-a-host)
- [Migrating from ipmi-fan-control](#migrating-from-ipmi-fan-control)

## Adding coolerd to a host

Add the flake as an input, then give the host the module and the overlay. The module takes the package from the overlay, so a host needs both.

```nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.coolerd.url = "github:mniehe/ipmi-fan-control";

  outputs = {
    nixpkgs,
    coolerd,
    ...
  }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        ./configuration.nix
        coolerd.nixosModules.default
        {nixpkgs.overlays = [coolerd.overlays.default];}
        # One of the recipes below, with its TOML file beside it
        ./coolerd/supermicro.nix
      ];
    };
  };
}
```

## Recipes

Each recipe is a NixOS module for one kind of host. It reads the TOML file beside it as `settings`. Copy the recipe and its TOML file into the configuration of your host, change the values marked `REPLACE`, and import the recipe.

| Host | Recipe | Config | Values to replace |
|---|---|---|---|
| Supermicro server | [`supermicro.nix`](examples/supermicro.nix) | [`supermicro.toml`](examples/supermicro.toml) | The sensor name, the drives and the curves. |
| Desktop or server without IPMI | [`hwmon-desktop.nix`](examples/hwmon-desktop.nix) | [`hwmon-desktop.toml`](examples/hwmon-desktop.toml) | The chip names, the PWM channel, the kernel module and the curve. `hwmon.pwmChannels` in the recipe must list every channel that the TOML file names. |
| NVIDIA GPU fans | [`nvidia.nix`](examples/nvidia.nix) | [`nvidia.toml`](examples/nvidia.toml) | The PCI bus id of the GPU and the curve. The driver package, when the host does not install the driver through `hardware.nvidia`. |
| Supermicro server with an NVIDIA GPU | [`mixed.nix`](examples/mixed.nix) | [`mixed.toml`](examples/mixed.toml) | The sensor name, the PCI bus id, the curves, and the driver package as above. |

`settings` can also be written in Nix, as the example under [`settings`](#servicescoolerdsettings) shows. [Configuration](configuration.md) describes every key.

## What the module sets up

With `services.coolerd.enable = true`, the module does this:

- It writes `settings` as TOML to `/etc/coolerd.toml`, with mode 0600 and the service user as owner.
- It runs `coolerd --config /etc/coolerd.toml` as the systemd unit `coolerd.service`, which starts at boot.
- It runs the unit as the system user and group `coolerd`. With [`enableNvidiaFans`](#servicescoolerdenablenvidiafans), the unit runs as root.
- It restarts the unit when `settings` changes.
- It restarts the unit 10 seconds after coolerd exits with a failure.
- It puts freeipmi on the `PATH` of the unit, plus the packages that the options below add.
- It adds the package to the system packages.

`systemctl stop coolerd.service` sends SIGTERM, so coolerd releases every fan as [Failures and shutdown](failure.md#what-the-fans-get-when-coolerd-stops) describes.

> **`settings` is world-readable.** NixOS copies `settings` into the Nix store, where every user of the host can read it. The mode of `/etc/coolerd.toml` does not protect that copy. Do not put a real BMC password in `settings`. A local session needs no password.

## Options

### `services.coolerd.enable`

Type: boolean. Default: `false`.

Turns the service on. `enable` alone does not give a working service: `settings` must hold a valid config with one zone or more. Without it, coolerd fails at each start and systemd starts it again every 10 seconds.

### `services.coolerd.package`

Type: package. Default: `pkgs.coolerd`, which the overlay provides.

The package to run. It must provide `bin/coolerd`. The system build fails when the package has no executable file `bin/coolerd`. The check does not run the program, so it does not prove that coolerd starts.

### `services.coolerd.settings`

Type: attribute set. Default: `{}`.

The config, in the shape that [Configuration](configuration.md) describes. The module does not check the keys; coolerd checks them when it starts. Read a TOML file:

```nix
services.coolerd.settings = builtins.fromTOML (builtins.readFile ./supermicro.toml);
```

or write the config in Nix:

```nix
services.coolerd.settings = {
  log_level = "info";
  zones = [
    {
      fans = [
        {
          type = "supermicro";
          zone = 0;
        }
      ];
      sources = [
        {
          type = "ipmi";
          sensor = "CPU Temp";
        }
      ];
      steps = [
        {
          temp = 40;
          dcycle = 40;
        }
        {
          temp = 80;
          dcycle = 100;
        }
      ];
    }
  ];
};
```

The build of the host fails when a zone has a fan of type `nvidia` and [`enableNvidiaFans`](#servicescoolerdenablenvidiafans) is off.

### `services.coolerd.enableSmartMonTools`

Type: boolean. Default: `false`.

Set it when `settings` has a `smart` source. It puts smartmontools on the `PATH` of the unit, adds the service user to the `disk` group, and gives the unit the capabilities `CAP_SYS_RAWIO` and `CAP_SYS_ADMIN`, which `smartctl` needs to query SATA and NVMe drives.

### `services.coolerd.enableHdparm`

Type: boolean. Default: `false`.

Set it when `settings` has an `hdparm` source. It puts hdparm on the `PATH` of the unit, adds the service user to the `disk` group, and gives the unit the capability `CAP_SYS_RAWIO`.

### `services.coolerd.extraPackages`

Type: list of packages. Default: `[]`. Example: `[(lib.getBin config.hardware.nvidia.package)]`.

More packages for the `PATH` of the unit. An `nvidia` source runs `nvidia-smi`, which must come from the driver that the host runs, so the module does not take it from `pkgs`. `config.hardware.nvidia.package` is the driver that `hardware.nvidia` installs. A host that installs the driver another way must name that package instead.

### `services.coolerd.enableIpmi`

Type: boolean. Default: `true`.

When it is set, the module loads the kernel modules `ipmi_devintf`, `ipmi_si` and `ipmi_msghandler`, creates the group `ipmi`, adds the service user to it, and adds a udev rule that gives the group the IPMI device. Set it to `false` on a host with no IPMI device. The module still puts freeipmi on the `PATH` of the unit.

### `services.coolerd.enableNvidiaFans`

Type: boolean. Default: `false`.

Set it when `settings` has a fan of type `nvidia`. It runs the unit as the user and group root, with no added capabilities, and makes root the owner of `/etc/coolerd.toml`. It also sets the `LD_LIBRARY_PATH` of the unit to `/run/opengl-driver/lib`. See [NVIDIA fans and root](#nvidia-fans-and-root).

### `services.coolerd.hwmon.kernelModules`

Type: list of strings. Default: `[]`. Example: `["nct6775"]`.

Kernel modules that the host loads at boot, for the hwmon chips that coolerd reads or drives. `nct6775` is the module of many Nuvoton Super I/O chips.

### `services.coolerd.hwmon.pwmChannels`

Type: attribute set of lists of positive integers. Default: `{}`. Example: `{nct6798 = [2 3];}`.

The PWM channels that the service may write, as a list of channel numbers for each chip name. List every channel that a `hwmon` fan in `settings` names; coolerd cannot take over a channel that is not listed. A chip name must match `[A-Za-z0-9_-]+`, or the build of the host fails. See [The hwmon access helper](#the-hwmon-access-helper).

## The hwmon access helper

The kernel makes `pwmN` and `pwmN_enable` writable by root only, and the service runs as the user `coolerd`. So a helper runs as root before each start of the service, when `hwmon.pwmChannels` is not empty.

For each chip in `hwmon.pwmChannels`, the helper finds the one entry of `/sys/class/hwmon` whose `name` file holds the chip name, after it trims surrounding whitespace. It skips an entry without a `name` file. It checks that `pwmN` and `pwmN_enable` of each listed channel are regular files and not symbolic links. Only when every file of every chip has passed does it give the service group write access to them. It changes no other file.

The helper fails, and the service does not start, in these cases:

- No entry or more than one entry has the chip name.
- A `name` file exists but cannot be read.
- A listed file is missing, is a symbolic link or is not a regular file.

Every check runs before the first grant, so a failed check grants nothing. The message of the helper names the chip or the path.

When `chgrp` or `chmod` fails while the helper grants access, the service does not start either. Some of the listed files can then already have the access.

The helper gives write access to the service group and not to all users, so that only the service can drive the fans. It runs before every start, so the access is set whenever the service starts, in whatever order the devices appeared.

The access has these limits:

- `pwmN` and `pwmN_enable` must be files in the chip's own hwmon directory. Neither coolerd nor the helper looks under `device/`.
- A grant lasts until the kernel removes the file. When you remove a channel from `hwmon.pwmChannels`, the service group keeps write access to it until a reboot or a reload of the driver.
- A reload of the hwmon driver removes the access. coolerd usually exits at its next read or write of the channel, and systemd restarts the service after 10 seconds, which runs the helper again. Until the restart, the fan stays under whatever control the reloaded driver sets, and coolerd cannot set full speed when it exits. Restart the service after you reload a hwmon driver. Do not rely on this recovery.

## NVIDIA fans and root

coolerd sets a GPU fan speed through NVML, and the module assumes that NVML accepts such a call only from root. So with `enableNvidiaFans` the whole service runs as root, and the sources and the other fans of the config run as root with it.

coolerd loads `libnvidia-ml.so.1` at run time, because the library must match the driver that the kernel has loaded. The library is therefore not an input of the package. The unit finds it through `LD_LIBRARY_PATH=/run/opengl-driver/lib`, where NixOS links the libraries of the graphics drivers.

`/run/opengl-driver/lib` exists only when the host enables its graphics driver packages, as `hardware.graphics.enable = true` does. Nothing warns when `enableNvidiaFans` is on and that directory is absent: the build of the host passes, and coolerd then fails when a zone with an `nvidia` fan starts. The [NVIDIA recipe](examples/nvidia.nix) sets `hardware.graphics.enable`.

An `nvidia` source also needs `nvidia-smi` on the `PATH` of the unit, through [`extraPackages`](#servicescoolerdextrapackages).

The NVIDIA fan backend has not yet been confirmed on a GPU. Check the fans after the first deployment, as the next section describes.

## Checking NVIDIA fans on a host

Run this check on the host itself, after a deployment that adds or changes an `nvidia` fan.

`nvidia-smi` cannot show the fan policy, that is, whether the driver or a program sets the speed. So these steps show only the effect. They suit a host with one zone that drives an NVIDIA fan. To make the change easy to see, give the zone a fixed duty cycle for the check, such as `steps = [{ temp = 0, dcycle = 60 }]`.

1. Start the unit with `sudo systemctl start coolerd.service`. Wait for a few updates, then read the speed:

   ```sh
   nvidia-smi --query-gpu=fan.speed --format=csv
   ```

   The speed is the duty cycle of the zone, or the fan's minimum when the duty cycle is lower. The unit logs the limits at the start in a line with `speed limits`. Read it with `journalctl -u coolerd.service`.
2. Stop the unit with `sudo systemctl stop coolerd.service`, and read the speed again with the same command. The speed follows the GPU temperature again.

If the journal shows a line that starts with `Failed to give fan`, the default policy was not restored. When that line ends with `under manual control`, the fan stays in manual control at its maximum. When it ends with `the fan's state is unknown`, the fan may keep the last speed that coolerd set, so start and stop the unit again or reload the driver.

## Migrating from ipmi-fan-control

The rename from ipmi-fan-control to coolerd changes these names:

| What | ipmi-fan-control | coolerd |
|---|---|---|
| Program | `ipmi-fan-control` | `coolerd` |
| systemd unit | `ipmi-fan-control.service` | `coolerd.service` |
| Service user and group | `ipmi-fan-control` | `coolerd` |
| Config file that the module writes | `/etc/ipmi-fan-control.toml` | `/etc/coolerd.toml` |
| NixOS options | `services.ipmi-fan-control` | `services.coolerd` |
| Nix package | `ipmi-fan-control` | `coolerd` |
| Log target in `RUST_LOG` | `ipmi_fan_control` | `coolerd` |
| Variable that turns off log timestamps | `IPMI_FAN_CONTROL_LOG_TIMESTAMPS` | `COOLERD_LOG_TIMESTAMPS` |

The format of the config file does not change, so an existing config loads with the same result. [Changes from earlier versions](configuration.md#changes-from-earlier-versions) lists the behaviour that differs.

The old option path still works. Each option under `services.ipmi-fan-control` is an alias of the same option under `services.coolerd`, and a priority such as `lib.mkForce` carries over. NixOS prints a warning for each old option that a configuration sets. `enableNvidiaFans` is new and exists only under `services.coolerd`. The Nix package `ipmi-fan-control` is the same package as `coolerd`. Move the configuration to `services.coolerd` to remove the warnings.

Before you switch a host to coolerd, note these points:

- Host settings that name the old unit, user or group do not carry over. Rename overrides written as `systemd.services.ipmi-fan-control.*`, drop-ins for `ipmi-fan-control.service`, and local udev rules or ACLs that name the `ipmi-fan-control` user or group.
- A normal system build fails when a `package` override has no executable file `bin/coolerd`.
- Move all definitions of one option to the new path together. The old and new paths define the same option. `lib.mkBefore` and `lib.mkAfter` keep their order among definitions that are all on the old path, but not relative to definitions on the new path.
- On the switch, NixOS stops `ipmi-fan-control.service`, which attempts to set the fans to full and to restore the saved fan mode as it exits. NixOS then starts `coolerd.service`.
- A started unit does not prove that coolerd controls the fans. Each zone logs `Starting loop for fans` before its first update. After the switch, watch `journalctl -u coolerd.service` for errors and restarts. At the default `log_level` of `info`, coolerd logs no line for each update. Set `log_level = "debug"` to see each one.
