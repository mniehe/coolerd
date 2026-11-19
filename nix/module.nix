# nix/module.nix
{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.services.ipmi-fan-control;

  # Service user and group
  serviceUser = "ipmi-fan-control";
  serviceGroup = "ipmi-fan-control";

  # Use pkgs.formats.toml to handle the conversion
  format = pkgs.formats.toml {};
  configFile = format.generate "ipmi-fan-control-config.toml" cfg.settings;

  # Runtime dependencies
  runtimeDeps = with pkgs;
    [
      freeipmi
    ]
    ++ optional cfg.enableSmartMonTools smartmontools
    ++ optional cfg.enableHdparm hdparm
    ++ cfg.extraPackages;

  # The helper grants group write rather than world write, so only the
  # service user can drive the fans. It runs before every start rather than
  # from a udev rule, because a udev rule does not reach a device that is
  # already present when the rule is installed.
  accessPreamble = ''
    set -eu

    if [ "$#" -ne 2 ]; then
      echo "usage: $0 HWMON_ROOT GROUP" >&2
      exit 2
    fi
    hwmon_root="$1"
    group="$2"

    # Set device to the one hwmon directory whose trimmed name is the chip,
    # the rule the daemon uses to find a chip
    find_chip() {
      chip="$1"
      device=""
      count=0
      for dir in "$hwmon_root"/*; do
        if [ ! -e "$dir/name" ]; then
          continue
        fi
        name=""
        # read fails at the end of a file with no newline but still sets name
        read -r name < "$dir/name" || true
        if [ "$name" = "$chip" ]; then
          device="$dir"
          count=$((count + 1))
        fi
      done
      if [ "$count" -ne 1 ]; then
        echo "hwmon chip $chip: expected one $hwmon_root entry with this name, found $count" >&2
        exit 1
      fi
    }

    grant() {
      if [ ! -e "$1" ]; then
        echo "$1 does not exist" >&2
        exit 1
      fi
      ${pkgs.coreutils}/bin/chgrp "$group" "$1"
      ${pkgs.coreutils}/bin/chmod g+w "$1"
    }
  '';

  grantChannel = n: ''
    grant "$device/pwm${toString n}"
    grant "$device/pwm${toString n}_enable"
  '';
  grantChip = chip: channels:
    ''
      find_chip ${escapeShellArg chip}
    ''
    + concatMapStrings grantChannel channels;

  accessHelper = pkgs.writeShellScript "ipmi-fan-control-hwmon-access" cfg.hwmon.accessScript;

  validChipName = chip: builtins.match "[A-Za-z0-9_-]+" chip != null;
in {
  options.services.ipmi-fan-control = {
    enable = mkEnableOption "IPMI fan control service";

    package = mkOption {
      type = types.package;
      default = pkgs.ipmi-fan-control;
      defaultText = literalExpression "pkgs.ipmi-fan-control";
      description = "The ipmi-fan-control package to use";
    };

    settings = mkOption {
      type = types.attrs;
      default = {};
      description = "Configuration for IPMI fan control";
      example = literalExpression ''
        {
          log_level = "info";
          zones = [
            {
              session = "default";
              fans = [
                {
                  type = "supermicro";
                  zone = 0;
                }
                {
                  type = "supermicro";
                  zone = 1;
                }
              ];
              # The older form of the same fans: ipmi_zones = [0 1];
              interval = 10;
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
          sessions = {
            default = {
              type = "local";
            };
          };
        }
      '';
    };

    enableSmartMonTools = mkOption {
      type = types.bool;
      default = false;
      description = "Enable support for querying drive temperatures via smartmontools";
    };

    enableHdparm = mkOption {
      type = types.bool;
      default = false;
      description = "Enable support for querying Hitachi/HGST/WD drive temperatures while spun down";
    };

    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [];
      example = literalExpression "[(lib.getBin config.hardware.nvidia.package)]";
      description = "Extra packages to put on the service's PATH, such as the package that provides nvidia-smi";
    };

    enableIpmi = mkOption {
      type = types.bool;
      default = true;
      description = "Load the IPMI kernel modules and give the service access to the IPMI device. Turn it off on a host with no IPMI and only hwmon fans";
    };

    hwmon = {
      kernelModules = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["nct6775"];
        description = "Kernel modules that provide the hwmon chips the service reads or drives";
      };

      pwmChannels = mkOption {
        type = types.attrsOf (types.listOf types.ints.positive);
        default = {};
        example = literalExpression "{nct6798 = [2 3];}";
        description = "PWM channels, by hwmon chip name, that the service may write. Before the service starts, its group gets write access to pwmN and pwmN_enable of each channel. The service fails to start when a chip is absent";
      };

      accessScript = mkOption {
        type = types.lines;
        internal = true;
        readOnly = true;
        description = "Text of the root helper that grants the service group write access to the pwmChannels files";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions =
      map (chip: {
        assertion = validChipName chip;
        message = "services.ipmi-fan-control.hwmon.pwmChannels: chip name ${builtins.toJSON chip} must match [A-Za-z0-9_-]+";
      })
      (attrNames cfg.hwmon.pwmChannels);

    services.ipmi-fan-control.hwmon.accessScript =
      accessPreamble
      + concatStrings (mapAttrsToList grantChip (filterAttrs (chip: _: validChipName chip) cfg.hwmon.pwmChannels));

    environment.systemPackages = [cfg.package];

    # Ensure the IPMI kernel modules are loaded
    boot.kernelModules = optionals cfg.enableIpmi ["ipmi_devintf" "ipmi_si" "ipmi_msghandler"] ++ cfg.hwmon.kernelModules;

    # Create the necessary groups
    users.groups = {
      # Main service group
      "${serviceGroup}" = {};

      # Ensure ipmi group exists
      "ipmi" = mkIf cfg.enableIpmi {};

      # Ensure disk group exists when drive access is needed
      "disk" = mkIf (cfg.enableSmartMonTools || cfg.enableHdparm) {};
    };

    # Create the service user with appropriate groups
    users.users."${serviceUser}" = {
      isSystemUser = true;
      group = serviceGroup;
      description = "IPMI Fan Control service user";
      home = "/var/empty";
      extraGroups =
        optional cfg.enableIpmi "ipmi"
        ++ optional cfg.enableSmartMonTools "disk"
        ++ optional cfg.enableHdparm "disk";
    };

    # Set up udev rules to ensure correct permissions for IPMI devices
    services.udev.extraRules = mkIf cfg.enableIpmi ''
      # Give the ipmi group access to the IPMI device
      KERNEL=="ipmi*", GROUP="ipmi", MODE="0660"
      KERNEL=="ipmidev/*", GROUP="ipmi", MODE="0660"
    '';

    environment.etc."ipmi-fan-control.toml" = {
      source = configFile;
      mode = "0600"; # Restrict access as it might contain credentials
      user = serviceUser;
      group = serviceGroup;
    };

    systemd.services.ipmi-fan-control = {
      description = "IPMI Fan Control Service";
      wantedBy = ["multi-user.target"];
      after = ["network.target"];
      restartTriggers = [configFile];

      path = runtimeDeps;

      serviceConfig = {
        Type = "simple";
        User = serviceUser;
        Group = serviceGroup;
        # The + prefix runs the helper as root; the daemon keeps its user
        ExecStartPre = mkIf (cfg.hwmon.pwmChannels != {}) "+${accessHelper} /sys/class/hwmon ${escapeShellArg serviceGroup}";
        ExecStart = "${cfg.package}/bin/ipmi-fan-control --config /etc/ipmi-fan-control.toml";
        Restart = "on-failure";
        RestartSec = "10s";
        # CAP_SYS_RAWIO: required for SATA SMART (SCSI passthrough) and hdparm
        # CAP_SYS_ADMIN: required for NVMe SMART (admin ioctl)
        AmbientCapabilities =
          lib.optionals (cfg.enableSmartMonTools || cfg.enableHdparm) ["CAP_SYS_RAWIO"]
          ++ lib.optionals cfg.enableSmartMonTools ["CAP_SYS_ADMIN"];
      };
    };
  };
}
