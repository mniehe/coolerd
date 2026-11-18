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

Each zone sets one duty cycle from one or more curves. The sources without their own `steps` are combined by `aggregation` into the zone temperature, which the zone's `steps` map to a duty cycle. A source with its own `steps` skips the zone temperature and maps its reading through its own curve; its `steps` must be non-empty. When every source has its own `steps`, the zone has no temperature: the zone's `steps` and `aggregation` are not used to compute the duty cycle, and the zone's `steps` may be omitted. Both are still validated when given.

The zone then combines the duty cycles of all its curves by `duty_aggregation`. `maximum`, the default, takes the highest duty cycle. `average` takes the mean, rounded up, and counts each curve once, however many sources feed it. The mean can be lower than the highest duty cycle, so the source behind the most demanding curve may run hotter than that curve intends.

An `average` `aggregation` divides by the number of temperatures it used; `top` only caps that number. Earlier versions divided by `top` even when the zone had fewer sources, so a zone whose `top` exceeds its source count now runs its fans faster: readings of 80 °C and 80 °C with `top = 3` give 80 °C, where they gave 53 °C.

`hysteresis` holds each curve's temperature through small falls. A curve follows a rise at once, but follows a fall only once it reaches `hysteresis` degrees. The default is 0, which follows every reading. The zone's `hysteresis` applies only to inline `steps`, the zone's and those of each source; a named curve, described below, has its own.

`smoothing_samples` on a source replaces each reading by the mean of the source's last `smoothing_samples` readings, rounded down, so a short spike moves the fans less. The mean comes first: the zone temperature, the source's own curve and `hysteresis` all see the smoothed reading. The default is 1, which uses each reading as it is, and 0 is rejected. The count is in updates, so the mean covers about `smoothing_samples` times `interval` seconds. When a source fails, its stored readings are dropped, and it starts again from fresh readings. With `smoothing_samples = 3` and steps from 40 °C at 40% to 80 °C at 100%, readings of 40, 70, 40 and 40 °C give 40%, 62%, 55% and 55%, where the same readings without smoothing give 40%, 85%, 40% and 40%.

Smoothing delays the response to real heat; `hysteresis` does not, because it follows every rise at once. The mean holds only the readings stored since the program started or since the source last failed, so the first reading after either acts at once. After that, the cooler readings still in the mean hold back a sustained rise until the `smoothing_samples`-th hot reading pushes out the last of them. The duty cycle can reach its target sooner, because the mean can cross the top step first: with `smoothing_samples = 3` and the steps above, three readings of 70 °C and then one of 100 °C give a mean of 80 °C and 100% at the first hot reading. Use smoothing for a sensor whose readings jump, as some AMD CPU and GPU sensors do, not for a device that heats quickly.

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

### Named curves

A curve can be defined once in the top-level `curves` table and named with `curve` by any number of sources and zones, in place of their own `steps`. A source or zone cannot have both `steps` and `curve`, and a name that `curves` does not hold is rejected. Each entry has `steps`, with the rules of a zone's `steps`, and an optional `hysteresis`, which defaults to 0. A named curve uses its own `hysteresis` and never the zone's.

A source or zone that names a curve behaves as if it had the curve's `steps` inline: the rules for its own `steps` apply, so a source cannot name a curve with no steps, and a source that names a curve does not feed the zone temperature. Each source that names a curve maps its own temperature and holds it on its own, so two GPUs on one curve do not share a held temperature.

In this example both GPUs use the curve `gpu`. With both at 60, 72, 70 and 68 °C in turn, the zone runs at 40%, 64%, 64% and 56%: the fall of 2 °C to 70 °C is held, and the fall of 4 °C to 68 °C is followed. The same `steps` inline on each GPU, with the zone's `hysteresis = 3`, give the same duty cycles.

```toml
[curves.gpu]
steps = [{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 100 }]
hysteresis = 3

[[zones]]
ipmi_zones = [0]
sources = [
  { type = "nvidia", gpu = "00000000:81:00.0", curve = "gpu" },
  { type = "nvidia", gpu = "00000000:82:00.0", curve = "gpu" },
]
```

See [`config.sample.toml`](config.sample.toml) for every key.

When a source fails
-------------------

A zone reads each source once per update. It reads the sources that are not `ipmi` first, in the order of `sources`, and then all `ipmi` sources in one IPMI query, so that the sensor readings are fresh when the zone sets its duty cycle. Each sensor in that query can fail on its own.

A `file` source's path must be a regular file after symlinks are followed, as sysfs and hwmon files are; a FIFO, directory or device fails the source without being read.

A source that fails is read again after `retry_delay_ms`, up to `retries` times; a source that read well is not read again. Earlier versions read every source of the zone again when one source failed. All sources of a zone share these retries in each update, so a source that uses them up leaves none for the sources read after it. `retries` bounds the number of waits in an update, not its duration, because each attempt also takes as long as the source needs to answer.

A source that failed in the previous update gets one attempt in each update and no retries, until it reads again. A source that keeps failing therefore leaves the retries to the other sources and adds one attempt to each update.

A program that does not answer in time is stopped: smartctl and hdparm after 30 seconds, nvidia-smi after 5 seconds. Earlier versions waited for smartctl and hdparm without a limit. Only that program is stopped, not the processes it started, so a wrapper script used in place of one of these tools must not leave helper processes behind. A stopped program can fail to exit, as nvidia-smi can in a hung driver. Its source then does not start the program again until the old process is gone, and until then that source fails at once. Each source tracks only its own process, so other sources still run, including sources that run the same program.

When a source still fails after the last retry, its `on_failure` decides what the zone does:

* `{ type = "exit" }`, the default: the program stops. On the way out it sets the fans of every zone to 100% and restores the original fan mode. Earlier versions handled every failure this way.
* `{ type = "ignore" }`: the zone leaves the source out. Its reading does not enter the zone temperature, and its own curve gives no duty cycle. The other sources keep driving the fans.
* `{ type = "dcycle", dcycle = N }`: the zone leaves the source out as with `ignore`, and runs at N% or more until the source reads again. The other curves can still raise the fans above N%.

A zone in which no source has a reading runs at 100%, whatever the modes. The program logs one error when a source starts to fail and one message when it reads again. A source that fails in every update is still tried once in every update. While it fails, its curve keeps its last temperature, so `hysteresis` applies from there when it reads again.

`ignore` has a risk: a hot device that cannot be read no longer raises the fans. Use `ignore` only for a source whose heat another source also measures, such as a second sensor on the same CPU. For a device that produces much heat, such as a GPU, use `dcycle` with a duty cycle that keeps the device cool under full load.

In this example the GPU falls back to 80%. With the CPU at 50 °C, a failed GPU runs the zone at 80%, where `ignore` would run it at 54%, the CPU curve's duty cycle. With the CPU at 75 °C, the zone runs at 100% either way.

```toml
[[zones]]
ipmi_zones = [0]
sources = [
  { type = "ipmi", sensor = "CPU Temp" },
  { type = "nvidia", gpu = "00000000:81:00.0", steps = [{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 100 }], on_failure = { type = "dcycle", dcycle = 80 } },
]
steps = [{ temp = 45, dcycle = 45 }, { temp = 75, dcycle = 100 }]
```

NixOS
-----

This flake provides a NixOS module, `nixosModules.default`, and an overlay, `overlays.default`, that adds the package. The module's options live under `services.ipmi-fan-control`. The module puts freeipmi on the service's `PATH`, plus smartmontools and hdparm when `enableSmartMonTools` and `enableHdparm` are set.

A source that runs another program needs that program on the service's `PATH`. Add its package to `extraPackages`. For example, to make `nvidia-smi` available:

```nix
services.ipmi-fan-control.extraPackages = [(lib.getBin config.hardware.nvidia.package)];
```

_Note_: The attribute that holds `nvidia-smi` depends on the host's driver package. `config.hardware.nvidia.package` is the driver that `hardware.nvidia` installs; a host that installs the driver another way must name that package instead.
