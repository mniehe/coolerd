ipmi-fan-control
================

ipmi-fan-control is a program written in Rust to control the fans on Supermicro motherboards based on the readings of temperature sensors.

_Note_: This has primarily been tested on a 6028U-TR4T+, which uses the X10DRU-i+ motherboard. Also, only Linux and other Unix-like operating systems are currently supported.

**2022-11-04 Update**: This project is no longer in active development because I no longer have a Supermicro motherboard that requires more granular fan control. However, the project is feature complete and I will continue to do maintenance work, like updating dependencies and creating binary repos for new Linux distro releases. The underlying fan control IPMI commands have not changed across 5 generations of motherboards. It's unlikely that ipmi-fan-control will stop working on any currently supported motherboards.

**2024-04-14 Update**: I am archiving this repo and no longer plan to keep the binary repos up to date for new distro releases. Folks who want to continue using this project should build it from source. As mentioned in the previous update, this project is feature complete and is unlikely to suddenly stop working.

Installation
------------

Prebuilt packages for Arch Linux, CentOS, Fedora, openSUSE, Debian, and Ubuntu are available from [OBS](https://build.opensuse.org/package/show/home:chenxiaolong:ipmi-fan-control/ipmi-fan-control). Please follow the instructions at [the repo's landing page](https://software.opensuse.org//download.html?project=home%3Achenxiaolong%3Aipmi-fan-control&package=ipmi-fan-control).

For other Unix-like systems, follow the instructions in the next section to build from source. Windows is currently not supported.

Building
--------

This project depends on:

* the freeipmi suite of libraries (specifically, libfreeipmi and libipmimonitoring)
* `pkg-config`
* the Clang compiler (for generating Rust FFI bindings to the freeipmi libraries)
* the Rust compiler
* [optional] smartmontools (for querying HDD/SSD drive temperatures)
* [optional] hdparm (for querying Hitachi/HGST/WD drive temperatures while spun down)
* [optional] nvidia-smi from the NVIDIA driver (for querying NVIDIA GPU temperatures). The `nvidia` source selects the card by PCI bus id or UUID; see [`config.sample.toml`](config.sample.toml). On NixOS, supply it through `extraPackages` as shown under [NixOS](#nixos).

These packages can be installed from the system package manager:

```sh
# Fedora
sudo dnf install freeipmi-devel pkgconf-pkg-config clang-devel cargo
# OpenSUSE
sudo zypper in freeipmi-devel pkgconf-pkg-config clang-devel cargo
# Arch Linux
sudo pacman -S freeipmi pkgconf clang cargo
# Debian-based distros
sudo apt install libfreeipmi-dev libipmimonitoring-dev pkg-config libclang-dev cargo
```

Then, to make a debug build, run:

```sh
cargo build
```

or to make a release build, run:

```sh
cargo build --release
```

To build Linux distro-specific packages, first build the corresponding source package:

```sh
# SRPM for RPM-based distros
./dist/build_source_package.py -t srpm
# PKGBUILD for Arch Linux
./dist/build_source_package.py -t pkgbuild
# dsc for Debian-based distros
./dist/build_source_package.py -t debian
```

and then use the distro's standard utilities for building the binary packages. The source packages will be placed in `dist/output/`.

Running
-------

If ipmi-fan-control was installed from a package, update `/etc/ipmi-fan-control.toml` to match the desired configuration and then enable and start the `ipmi-fan-control` systemd service.

If built from source, make a copy of [`config.sample.toml`](config.sample.toml) and update the values to match your server's configuration. Then, run `ipmi-fan-control` with:

```sh
# Debug
sudo ./target/debug/ipmi-fan-control --config config.toml
# Release
sudo ./target/release/ipmi-fan-control --config config.toml
```

Fan curves
----------

Each zone sets one duty cycle from one or more curves. The sources without their own `steps` are combined by `aggregation` into the zone temperature, which the zone's `steps` map to a duty cycle. A source with its own `steps` skips the zone temperature and maps its reading through its own curve; its `steps` must be non-empty. When every source has its own `steps`, the zone has no temperature: the zone's `steps` and `aggregation` are ignored, and the zone's `steps` may be omitted.

The zone then combines the duty cycles of all its curves by `duty_aggregation`. `maximum`, the default, takes the highest duty cycle. `average` takes the mean, rounded up, and counts each curve once, however many sources feed it. The mean can be lower than the highest duty cycle, so the source behind the most demanding curve may run hotter than that curve intends.

An `average` `aggregation` divides by the number of temperatures it used; `top` only caps that number. Earlier versions divided by `top` even when the zone had fewer sources, so a zone whose `top` exceeds its source count now runs its fans faster: readings of 80 °C and 80 °C with `top = 3` give 80 °C, where they gave 53 °C.

`hysteresis` holds each curve's temperature through small falls. A curve follows a rise at once, but follows a fall only once it reaches `hysteresis` degrees. The default is 0, which follows every reading.

In this example the CPU uses the zone curve and the GPU its own. At CPU 50 °C and GPU 70 °C, the curves give 54% and 60%, and the zone runs at 60% on the first poll. On later polls, `hysteresis` can hold a curve's temperature above its reading, so the duty cycle can be higher than the current readings alone give.

```toml
[[zones]]
ipmi_zones = [0]
sources = [
  { type = "ipmi", sensor = "CPU Temp" },
  { type = "nvidia", gpu = "00000000:81:00.0", steps = [{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 100 }] },
]
steps = [{ temp = 45, dcycle = 45 }, { temp = 75, dcycle = 100 }]
duty_aggregation = { type = "maximum" }
hysteresis = 3
```

See [`config.sample.toml`](config.sample.toml) for every key.

NixOS
-----

This flake provides a NixOS module, `nixosModules.default`, and an overlay, `overlays.default`, that adds the package. The module's options live under `services.ipmi-fan-control`. The module puts freeipmi on the service's `PATH`, plus smartmontools and hdparm when `enableSmartMonTools` and `enableHdparm` are set.

A source that runs another program needs that program on the service's `PATH`. Add its package to `extraPackages`. For example, to make `nvidia-smi` available:

```nix
services.ipmi-fan-control.extraPackages = [(lib.getBin config.hardware.nvidia.package)];
```

_Note_: The attribute that holds `nvidia-smi` depends on the host's driver package. `config.hardware.nvidia.package` is the driver that `hardware.nvidia` installs; a host that installs the driver another way must name that package instead.
