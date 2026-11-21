{
  description = "coolerd";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [self.overlays.default];
        };
        lib = nixpkgs.lib;

        # Evaluate a minimal host with the module and the given modules, which
        # must enable the service themselves
        evalModules = modules:
          lib.nixosSystem {
            modules =
              [
                self.nixosModules.default
                {
                  nixpkgs.hostPlatform = system;
                  nixpkgs.overlays = [self.overlays.default];
                  fileSystems."/".device = "/dev/null";
                  boot.loader.grub.enable = false;
                }
              ]
              ++ modules;
          };
        # Evaluate a minimal host that enables the service, plus one extra module
        evalHost = extraModule: evalModules [{services.coolerd.enable = true;} extraModule];

        # Settings that the migration host sets on the old option path
        migrationSettings = {
          log_level = "debug";
          zones = [
            {
              fans = [
                {
                  type = "hwmon";
                  chip = "nct6798";
                  pwm = 2;
                }
              ];
              sources = [
                {
                  type = "hwmon";
                  chip = "k10temp";
                  input = 1;
                }
              ];
              steps = [
                {
                  temp = 40;
                  dcycle = 30;
                }
              ];
            }
          ];
        };
        # Differs from migrationSettings in a shared key and an extra key, so a
        # merge of the two cannot equal migrationSettings
        migrationRival = {
          log_level = "info";
          sessions = {};
        };
        # A value other than the default for every other public leaf, so an old
        # path that sets nothing changes what the host builds
        migrationLeaves = pkgs: {
          package = inertPackage;
          enableIpmi = false;
          enableSmartMonTools = true;
          enableHdparm = true;
          extraPackages = [pkgs.hello];
          hwmon.kernelModules = ["nct6775"];
          hwmon.pwmChannels.nct6798 = [2];
        };
        migrationExpectedPath = p: [p.freeipmi p.smartmontools p.hdparm p.hello];

        # The library directory a host's graphics driver packages provide
        nvidiaLibraryPath = "/run/opengl-driver/lib";
        # A zone that drives an NVIDIA fan, which needs enableNvidiaFans
        nvidiaZone = {
          fans = [
            {
              type = "nvidia";
              gpu = "00000000:01:00.0";
              fan = 0;
            }
          ];
          sources = [
            {
              type = "nvidia";
              gpu = "00000000:01:00.0";
            }
          ];
          steps = [
            {
              temp = 50;
              dcycle = 30;
            }
          ];
        };

        # The capabilities of the unit for each combination of the options that
        # set them. ambient lists AmbientCapabilities; bounding is the value of
        # CapabilityBoundingSet, or null when the unit sets none. Each row is
        # written out, so a change to the module's rule must change a row.
        capabilityCases = [
          {
            nvidia = false;
            smart = false;
            hdparm = false;
            ambient = [];
            bounding = null;
          }
          {
            nvidia = false;
            smart = true;
            hdparm = false;
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
            bounding = null;
          }
          {
            nvidia = false;
            smart = false;
            hdparm = true;
            ambient = ["CAP_SYS_RAWIO"];
            bounding = null;
          }
          {
            nvidia = false;
            smart = true;
            hdparm = true;
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
            bounding = null;
          }
          {
            nvidia = true;
            smart = false;
            hdparm = false;
            ambient = [];
            bounding = "CAP_SYS_ADMIN";
          }
          {
            nvidia = true;
            smart = true;
            hdparm = false;
            ambient = [];
            bounding = "CAP_SYS_ADMIN CAP_SYS_RAWIO";
          }
          {
            nvidia = true;
            smart = false;
            hdparm = true;
            ambient = [];
            bounding = "CAP_SYS_ADMIN CAP_SYS_RAWIO";
          }
          {
            nvidia = true;
            smart = true;
            hdparm = true;
            ambient = [];
            bounding = "CAP_SYS_ADMIN CAP_SYS_RAWIO";
          }
        ];
        onOff = flag:
          if flag
          then "on"
          else "off";
        # One host per row of capabilityCases
        capabilityHosts = lib.listToAttrs (map (case: {
            name = "capabilities: nvidia ${onOff case.nvidia}, smart ${onOff case.smart}, hdparm ${onOff case.hdparm}";
            value =
              {
                nixos = evalHost {
                  services.coolerd = {
                    enableNvidiaFans = case.nvidia;
                    enableSmartMonTools = case.smart;
                    enableHdparm = case.hdparm;
                  };
                };
                expected = p: [p.freeipmi] ++ lib.optional case.smart p.smartmontools ++ lib.optional case.hdparm p.hdparm;
                inherit (case) ambient bounding;
              }
              // lib.optionalAttrs case.nvidia {
                user = "root";
                libraryPath = nvidiaLibraryPath;
              };
          })
          capabilityCases);

        # Each host names the packages whose bin directory must be on the unit's
        # PATH. A host runs as the user and group coolerd and sets no
        # LD_LIBRARY_PATH unless it names another user or a libraryPath. Its
        # unit has no AmbientCapabilities and no CapabilityBoundingSet unless
        # it names an ambient list or a bounding value.
        hosts =
          capabilityHosts
          // namedHosts;
        namedHosts = {
          default = {
            nixos = evalHost {};
            expected = p: [p.freeipmi];
          };
          extra = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd.extraPackages = [pkgs.hello];
            });
            expected = p: [p.freeipmi p.hello];
          };
          drives = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd = {
                enableSmartMonTools = true;
                enableHdparm = true;
                extraPackages = [pkgs.hello];
              };
            });
            expected = p: [p.freeipmi p.smartmontools p.hdparm p.hello];
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
          };
          hwmon = {
            nixos = evalHost {
              services.coolerd = {
                enableIpmi = false;
                hwmon.kernelModules = ["nct6775"];
                hwmon.pwmChannels.nct6798 = [2 12];
                settings.zones = [
                  {
                    fans = [
                      {
                        type = "hwmon";
                        chip = "nct6798";
                        pwm = 2;
                      }
                      {
                        type = "hwmon";
                        chip = "nct6798";
                        pwm = 12;
                      }
                    ];
                    sources = [
                      {
                        type = "hwmon";
                        chip = "k10temp";
                        input = 1;
                      }
                    ];
                    steps = [
                      {
                        temp = 40;
                        dcycle = 30;
                      }
                      {
                        temp = 80;
                        dcycle = 100;
                      }
                    ];
                  }
                ];
              };
            };
            expected = p: [p.freeipmi];
          };
          # NVML fan commands need root. The drive options would otherwise set
          # AmbientCapabilities, and pwmChannels makes the unit run the helper;
          # the channels are the hwmon host's, so both hosts run the one helper
          # that hwmon-access exercises.
          nvidia = {
            nixos = evalHost {
              services.coolerd = {
                enableNvidiaFans = true;
                enableSmartMonTools = true;
                enableHdparm = true;
                hwmon.pwmChannels.nct6798 = [2 12];
                settings.zones = [nvidiaZone];
              };
            };
            expected = p: [p.freeipmi p.smartmontools p.hdparm];
            user = "root";
            libraryPath = nvidiaLibraryPath;
            bounding = "CAP_SYS_ADMIN CAP_SYS_RAWIO";
          };
          # Old option paths, as a configuration written before the rename
          # sets them. The normal-priority new-path definitions compete with
          # them, so mkForce on an old path must carry over to win.
          migration = {
            nixos = evalModules [
              {services.coolerd.enable = false;}
              {services.coolerd.settings = migrationRival;}
              {services.ipmi-fan-control.enable = lib.mkForce true;}
              {services.ipmi-fan-control.settings = lib.mkForce migrationSettings;}
              ({pkgs, ...}: {services.ipmi-fan-control = migrationLeaves pkgs;})
            ];
            expected = migrationExpectedPath;
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
          };
          # The migration host's values through the new option paths
          migrationTwin = {
            nixos = evalHost ({pkgs, ...}: {
              services.coolerd = migrationLeaves pkgs // {settings = migrationSettings;};
            });
            expected = migrationExpectedPath;
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
          };
          # The recipes of docs/examples. Each enables the service itself and
          # reads the TOML file beside it as its settings.
          recipeSupermicro = {
            recipe = "supermicro";
            nixos = evalModules [./docs/examples/supermicro.nix];
            expected = p: [p.freeipmi p.smartmontools];
            ambient = ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"];
          };
          recipeHwmonDesktop = {
            recipe = "hwmon-desktop";
            nixos = evalModules [./docs/examples/hwmon-desktop.nix];
            expected = p: [p.freeipmi];
          };
          recipeNvidia = {
            recipe = "nvidia";
            nixos = evalModules [./docs/examples/nvidia.nix driverStandIn];
            expected = p: [p.freeipmi p.hello];
            user = "root";
            libraryPath = nvidiaLibraryPath;
            bounding = "CAP_SYS_ADMIN";
          };
          recipeMixed = {
            recipe = "mixed";
            nixos = evalModules [./docs/examples/mixed.nix driverStandIn];
            expected = p: [p.freeipmi p.hello];
            user = "root";
            libraryPath = nvidiaLibraryPath;
            bounding = "CAP_SYS_ADMIN";
          };
        };

        # The NVIDIA driver is unfree and is not evaluated here, so a free
        # package stands in for the one a recipe puts on the unit's PATH
        driverStandIn = {pkgs, ...}: {
          hardware.nvidia.package = pkgs.hello;
        };

        # A host whose chip name the module must reject. Only its assertions are
        # read: its toplevel and unit would fail or drop the chip.
        badChipConfig =
          (evalHost {
            services.coolerd.hwmon.pwmChannels."nct*" = [2];
          })
          .config;
        rejectsBadChip =
          lib.any (a: !a.assertion && lib.hasInfix "services.coolerd.hwmon.pwmChannels" a.message)
          badChipConfig.assertions;

        # Hosts that leave enableNvidiaFans off, each with one zone. Only their
        # assertions are read.
        failedWithZone = zone:
          map (a: a.message)
          (lib.filter (a: !a.assertion && lib.hasInfix "services.coolerd" a.message)
            (evalHost {services.coolerd.settings.zones = [zone];}).config.assertions);
        rejectsWithoutNvidiaOption = zone: lib.any (lib.hasInfix "services.coolerd.enableNvidiaFans") (failedWithZone zone);
        # A source that is not an NVIDIA one, so only the fan can need the option
        hwmonSource = {
          type = "hwmon";
          chip = "k10temp";
          input = 1;
        };
        nvidiaFanTableZone =
          nvidiaZone
          // {
            sources = [hwmonSource];
          };
        # The fan as a list in field order, which the daemon also accepts
        nvidiaFanPositionalZone =
          nvidiaFanTableZone
          // {
            fans = [["nvidia" "00000000:01:00.0" 0]];
          };
        nvidiaSourceOnlyZone =
          nvidiaZone
          // {
            fans = [
              {
                type = "hwmon";
                chip = "nct6798";
                pwm = 2;
              }
            ];
          };
        # The message of each failed assertion of the module on a host
        failedAssertions = host:
          map (a: a.message)
          (lib.filter (a: !a.assertion && lib.hasInfix "services.coolerd" a.message) host.nixos.config.assertions);

        # A host with two chips. Only its access script is read, so that the
        # helper can be run against a tree where the second chip fails.
        twoChipScript =
          (evalHost {
            services.coolerd.hwmon.pwmChannels = {
              nct6798 = [2];
              it8792 = [1];
            };
          })
          .config
          .services
          .coolerd
          .hwmon
          .accessScript;

        # A package that ships only the program name from before the rename
        oldNamePackage = pkgs.runCommand "old-name-package" {} ''
          mkdir -p $out/bin
          printf '#!/bin/sh\n' > $out/bin/ipmi-fan-control
          chmod +x $out/bin/ipmi-fan-control
        '';
        # A package with an executable bin/coolerd that is never run, so the
        # package check can be built without building the Rust package
        inertPackage = pkgs.runCommand "coolerd-inert-package" {} ''
          mkdir -p $out/bin
          printf '#!/bin/sh\necho "inert test package, not coolerd" >&2\nexit 1\n' > $out/bin/coolerd
          chmod +x $out/bin/coolerd
        '';
        # The module's package check of a host, or an empty stand-in when it has none
        packageCheck = config:
          lib.findFirst (c: c.name == "coolerd-package-check") {
            buildCommand = "";
            package = null;
          }
          config.system.checks;
        # Only the old-name host's package check is read: its system build must fail
        oldNameCheck = packageCheck (evalHost {services.coolerd.package = oldNamePackage;}).config;
        # Of inertPackage, which the migration host sets on the old path
        migrationCheck = packageCheck hosts.migration.nixos.config;
        # Only evaluated: building it would build the Rust package
        defaultCheck = packageCheck hosts.default.nixos.config;
        nvidiaCheck = packageCheck hosts.nvidia.nixos.config;
        checksPackage = check: expected: check.package != null && "${check.package}" == "${expected}";

        hasPackage = name: packages: lib.any (p: lib.getName p == name) packages;
        unit = host: host.nixos.config.systemd.services.coolerd;
        unitText = host: host.nixos.config.systemd.units."coolerd.service".text;

        # PATH= at the start of a value or after a space or quote, so LD_LIBRARY_PATH= does not count
        assignsPath = s: builtins.match "(.*[ \"])?PATH=.*" s != null;
        envSetsPath = host:
          lib.any (v: lib.isString v && assignsPath v)
          (lib.toList ((unit host).serviceConfig.Environment or []));
        pathLines = host:
          lib.filter (line: lib.hasPrefix "Environment=" line && assignsPath (lib.removePrefix "Environment=" line))
          (lib.splitString "\n" (unitText host));

        # environment.PATH is what the unit file sets; a forced value there bypasses the path option
        effectivePath = host: lib.splitString ":" ((unit host).environment.PATH or "");
        missingFromPath = host:
          lib.filter (p: !lib.elem (lib.makeBinPath [p]) (effectivePath host))
          (host.expected host.nixos.pkgs);

        defaultConfig = hosts.default.nixos.config;
        ipmiModules = ["ipmi_devintf" "ipmi_si" "ipmi_msghandler"];
        hasIpmiModule = kernelModules: lib.any (m: lib.elem m kernelModules) ipmiModules;
        # Each rule as one whole line, so its GROUP and MODE apply to its KERNEL selector
        ipmiUdevRules = [
          "KERNEL==\"ipmi*\", GROUP=\"ipmi\", MODE=\"0660\""
          "KERNEL==\"ipmidev/*\", GROUP=\"ipmi\", MODE=\"0660\""
        ];

        hwmonConfig = hosts.hwmon.nixos.config;
        migrationConfig = hosts.migration.nixos.config;
        # The path of every option leaf under opts that is neither internal
        # nor hidden
        publicLeaves = prefix: opts:
          lib.concatLists (lib.mapAttrsToList (name: value:
            if lib.isOption value
            then lib.optional (!(value.internal or false) && (value.visible or true) != false) (prefix ++ [name])
            else publicLeaves (prefix ++ [name]) value)
          opts);
        defaultOptions = hosts.default.nixos.options.services;
        # Options added after the rename; no configuration from before it can
        # set them, so they have no old-name alias
        postRenameLeaves = [["enableNvidiaFans"]];
        unknownPostRenameLeaves = lib.filter (leaf: !lib.elem leaf (publicLeaves [] defaultOptions.coolerd)) postRenameLeaves;
        renamedLeaves = lib.filter (leaf: !lib.elem leaf postRenameLeaves) (publicLeaves [] defaultOptions.coolerd);
        unaliasedLeaves = lib.filter (leaf: !lib.hasAttrByPath leaf defaultOptions.ipmi-fan-control) renamedLeaves;
        # Leaves the migration host leaves unset on the old path, so their
        # forwarding goes unchecked
        migrationOldOptions = hosts.migration.nixos.options.services.ipmi-fan-control;
        # The values the migration host sets on the old path
        migrationOld = migrationLeaves hosts.migration.nixos.pkgs;
        unsetOldLeaves =
          lib.filter (leaf: !(lib.attrByPath leaf {isDefined = false;} migrationOldOptions).isDefined)
          renamedLeaves;
        # What the options produce on a host, for comparing the old-path and new-path hosts
        producedBy = host: let
          config = host.nixos.config;
          etc = config.environment.etc."coolerd.toml";
        in {
          "the unit file" = unitText host;
          "the service user" = {inherit (config.users.users.coolerd) group extraGroups isSystemUser home;};
          "users.groups" = lib.attrNames config.users.groups;
          "the udev rules" = config.services.udev.extraRules;
          "boot.kernelModules" = config.boot.kernelModules;
          "/etc/coolerd.toml" = {
            inherit (etc) mode user group;
            source = "${etc.source}";
          };
        };
        migrationDifferences = let
          twin = producedBy hosts.migrationTwin;
        in
          lib.attrNames (lib.filterAttrs (name: value: value != twin.${name}) (producedBy hosts.migration));
        # The config file the module would generate from the old-path settings
        migrationExpected = (pkgs.formats.toml {}).generate "coolerd-config.toml" migrationSettings;
        accessScript = hwmonConfig.services.coolerd.hwmon.accessScript;
        # The first capture of pattern on each line of script that matches it, in order
        scriptMatches = pattern: script:
          lib.concatMap (line: let
            m = builtins.match pattern line;
          in
            lib.optional (m != null) (lib.head m))
          (lib.splitString "\n" script);
        # The file each add_target line of an access helper names
        filesGranted = scriptMatches "[[:space:]]*add_target \"\\$device/(pwm[0-9]+(_enable)?)\"";
        # The chip each find_chip call of an access helper names
        chipsFound = scriptMatches "find_chip (.*)";
        grantedFiles = filesGranted accessScript;
        execStartPre = host: lib.toList ((unit host).serviceConfig.ExecStartPre or []);
        # The ExecStartPre that runs helper with the + prefix on the hwmon root
        # for group, as the module builds it
        accessCommands = helper: group: ["+${helper} /sys/class/hwmon ${lib.escapeShellArg group}"];
        # The module builds the helper the same way, so on the hwmon host it is
        # this very derivation
        accessHelper = pkgs.writeShellScript "coolerd-hwmon-access" accessScript;
        migrationHelper = pkgs.writeShellScript "coolerd-hwmon-access" migrationConfig.services.coolerd.hwmon.accessScript;
        # The derivations that a host's ExecStartPre commands run
        execStartPreDrvs = host: lib.concatMap (command: lib.attrNames (builtins.getContext command)) (execStartPre host);
        twoChipHelper = pkgs.writeShellScript "hwmon-access-helper-two-chips" twoChipScript;

        # A public option is documented by a heading of docs/nixos.md that is
        # this whole line; the option's name in prose does not count
        optionHeading = leaf: "### `services.coolerd.${lib.concatStringsSep "." leaf}`";
        nixosPageLines = lib.splitString "\n" (builtins.readFile ./docs/nixos.md);
        undocumentedLeaves = lib.filter (leaf: !lib.elem (optionHeading leaf) nixosPageLines) (publicLeaves [] defaultOptions.coolerd);

        recipeHosts = lib.filterAttrs (_: host: host ? recipe) hosts;
        sortedChannels = lib.mapAttrs (_: lib.sort lib.lessThan);
        # The PWM channels, by chip, of the hwmon fans that settings lists
        settingsPwmChannels = settings:
          sortedChannels (lib.zipAttrsWith (_: lib.unique)
            (map (fan: {${fan.chip} = fan.pwm;})
              (lib.filter (fan: fan.type == "hwmon")
                (lib.concatMap (zone: zone.fans or []) (settings.zones or [])))));
        # A recipe's settings are its TOML file, its pwmChannels are the hwmon
        # fans of that file, and its unit runs the access helper for them
        recipeFailures = name: host: let
          cfg = host.nixos.config.services.coolerd;
          helper = pkgs.writeShellScript "coolerd-hwmon-access" cfg.hwmon.accessScript;
          expectedPre =
            if cfg.hwmon.pwmChannels == {}
            then []
            else accessCommands helper (hostUser host);
        in
          lib.optional (cfg.settings != builtins.fromTOML (builtins.readFile (./docs/examples + "/${host.recipe}.toml")))
          "${name}: settings is not docs/examples/${host.recipe}.toml"
          ++ lib.optional (sortedChannels cfg.hwmon.pwmChannels != settingsPwmChannels cfg.settings)
          "${name}: hwmon.pwmChannels is ${builtins.toJSON cfg.hwmon.pwmChannels}, but the hwmon fans of docs/examples/${host.recipe}.toml are ${builtins.toJSON (settingsPwmChannels cfg.settings)}"
          ++ lib.optional (execStartPre host != expectedPre)
          "${name}: ExecStartPre is ${shown (execStartPre host)}, expected ${shown expectedPre}";
        # A file added to docs/examples without a recipe host above would
        # otherwise pass this check unevaluated
        exampleNames = suffix:
          lib.sort lib.lessThan (map (lib.removeSuffix suffix)
            (lib.filter (lib.hasSuffix suffix) (lib.attrNames (builtins.readDir ./docs/examples))));
        hostedRecipes = lib.sort lib.lessThan (lib.mapAttrsToList (_: host: host.recipe) recipeHosts);
        unhostedFailures =
          lib.optional (exampleNames ".nix" != hostedRecipes)
          "docs/examples holds the recipes ${builtins.toJSON (exampleNames ".nix")}, but the check hosts evaluate ${builtins.toJSON hostedRecipes}";
        recipeConfig = host: host.nixos.config;

        configFileAccess = host: {inherit (host.nixos.config.environment.etc."coolerd.toml") user group mode;};
        hostUser = host: host.user or "coolerd";
        # Readable only by the user the unit runs as
        unitOnlyAccess = host: {
          user = hostUser host;
          group = hostUser host;
          mode = "0600";
        };

        hostLibraryPath = host: host.libraryPath or null;
        libraryPathLines = host:
          lib.filter (line: lib.hasPrefix "Environment=" line && lib.hasInfix "LD_LIBRARY_PATH" line)
          (lib.splitString "\n" (unitText host));
        expectedLibraryPathLines = host:
          lib.optional (hostLibraryPath host != null) "Environment=\"LD_LIBRARY_PATH=${hostLibraryPath host}\"";

        # The lines of the unit file that assign key, in order
        unitLines = key: host: lib.filter (lib.hasPrefix "${key}=") (lib.splitString "\n" (unitText host));
        expectedAmbientLines = host: map (capability: "AmbientCapabilities=${capability}") (host.ambient or []);
        expectedBoundingLines = host: lib.optional ((host.bounding or null) != null) "CapabilityBoundingSet=${host.bounding}";
        # Only a root unit is kept from gaining privileges
        expectedNoNewPrivilegesLines = host: lib.optional (hostUser host == "root") "NoNewPrivileges=true";

        # A value for a failure message, without its store path context, so a
        # failure that names the Rust package does not build it
        shown = value: builtins.unsafeDiscardStringContext (builtins.toJSON value);

        # ExecStart that runs bin/coolerd of the package the host's package
        # check gates, or null when the host has no package check
        expectedExecStart = host: let
          gated = (packageCheck host.nixos.config).package;
        in
          if gated == null
          then null
          else "${gated}/bin/coolerd --config /etc/coolerd.toml";

        hostFailures = name: host:
          map (p: "${name}: ${lib.getName p} is not on the effective unit PATH") (missingFromPath host)
          ++ lib.optional (envSetsPath host)
          "${name}: serviceConfig.Environment sets PATH"
          ++ lib.optional (lib.length (pathLines host) != 1)
          "${name}: the unit file does not assign PATH exactly once"
          ++ lib.optional ((unit host).serviceConfig.User != hostUser host || (unit host).serviceConfig.Group != hostUser host)
          "${name}: the unit does not run as the user and group ${hostUser host}"
          ++ lib.optional (((unit host).environment.LD_LIBRARY_PATH or null) != hostLibraryPath host)
          "${name}: the unit's environment.LD_LIBRARY_PATH is ${builtins.toJSON ((unit host).environment.LD_LIBRARY_PATH or null)}, expected ${builtins.toJSON (hostLibraryPath host)}"
          ++ lib.optional (libraryPathLines host != expectedLibraryPathLines host)
          "${name}: the unit file's LD_LIBRARY_PATH lines are ${builtins.toJSON (libraryPathLines host)}, expected ${builtins.toJSON (expectedLibraryPathLines host)}"
          ++ lib.optional ((unit host).serviceConfig.ExecStart != expectedExecStart host)
          "${name}: ExecStart is ${shown (unit host).serviceConfig.ExecStart}, expected ${shown (expectedExecStart host)}, bin/coolerd of the package that system.checks gates"
          ++ lib.optional (!(host.nixos.config.environment.etc ? "coolerd.toml"))
          "${name}: /etc/coolerd.toml is not defined"
          ++ lib.optional (configFileAccess host != unitOnlyAccess host)
          "${name}: /etc/coolerd.toml is not owned by ${hostUser host}:${hostUser host} with mode 0600"
          ++ lib.optional (unitLines "AmbientCapabilities" host != expectedAmbientLines host)
          "${name}: the unit file's AmbientCapabilities lines are ${builtins.toJSON (unitLines "AmbientCapabilities" host)}, expected ${builtins.toJSON (expectedAmbientLines host)}"
          ++ lib.optional (unitLines "CapabilityBoundingSet" host != expectedBoundingLines host)
          "${name}: the unit file's CapabilityBoundingSet lines are ${builtins.toJSON (unitLines "CapabilityBoundingSet" host)}, expected ${builtins.toJSON (expectedBoundingLines host)}"
          ++ lib.optional (unitLines "NoNewPrivileges" host != expectedNoNewPrivilegesLines host)
          "${name}: the unit file's NoNewPrivileges lines are ${builtins.toJSON (unitLines "NoNewPrivileges" host)}, expected ${builtins.toJSON (expectedNoNewPrivilegesLines host)}"
          # The unit text omits wantedBy, so the twin comparison cannot see it
          ++ lib.optional (!lib.elem "multi-user.target" (unit host).wantedBy)
          "${name}: multi-user.target does not want the unit"
          ++ map (message: "${name}: an assertion fails: ${message}") (failedAssertions host);

        moduleFailures =
          lib.concatLists (lib.mapAttrsToList hostFailures hosts)
          ++ map (leaf: "docs/nixos.md: no heading ${optionHeading leaf}") undocumentedLeaves
          ++ lib.concatLists (lib.mapAttrsToList recipeFailures recipeHosts)
          ++ unhostedFailures
          ++ lib.optional (!((recipeConfig hosts.recipeSupermicro).users.groups ? ipmi) || !lib.elem "ipmi" (recipeConfig hosts.recipeSupermicro).users.users.coolerd.extraGroups)
          "recipeSupermicro: the service user is not in an ipmi group"
          ++ lib.optional (!lib.all (m: lib.elem m (recipeConfig hosts.recipeSupermicro).boot.kernelModules) ipmiModules)
          "recipeSupermicro: boot.kernelModules does not contain the IPMI modules"
          ++ lib.optional ((recipeConfig hosts.recipeHwmonDesktop).services.coolerd.hwmon.pwmChannels == {})
          "recipeHwmonDesktop: hwmon.pwmChannels is empty"
          ++ lib.optional (!lib.all (m: lib.elem m (recipeConfig hosts.recipeHwmonDesktop).boot.kernelModules) (recipeConfig hosts.recipeHwmonDesktop).services.coolerd.hwmon.kernelModules || (recipeConfig hosts.recipeHwmonDesktop).services.coolerd.hwmon.kernelModules == [])
          "recipeHwmonDesktop: boot.kernelModules does not contain a hwmon module of the recipe"
          ++ lib.optional (hasIpmiModule (recipeConfig hosts.recipeHwmonDesktop).boot.kernelModules || hasIpmiModule (recipeConfig hosts.recipeNvidia).boot.kernelModules)
          "recipeHwmonDesktop, recipeNvidia: an IPMI kernel module is present with enableIpmi = false"
          ++ lib.optional (!lib.all (m: lib.elem m (recipeConfig hosts.recipeMixed).boot.kernelModules) ipmiModules)
          "recipeMixed: boot.kernelModules does not contain the IPMI modules"
          ++ lib.optional (!(recipeConfig hosts.recipeNvidia).hardware.graphics.enable || !(recipeConfig hosts.recipeMixed).hardware.graphics.enable)
          "recipeNvidia, recipeMixed: hardware.graphics.enable is off, so /run/opengl-driver/lib would not exist"
          ++ lib.optional (!hasPackage "hello" (unit hosts.extra).path)
          "extraPackages: hello is not in the unit path"
          ++ lib.optional (!hasPackage "freeipmi" (unit hosts.default).path)
          "default: freeipmi is not in the unit path"
          ++ lib.optional (hosts.default.nixos.config.services.coolerd.extraPackages != [])
          "default: extraPackages is not empty"
          ++ lib.optional (!lib.all (m: lib.elem m defaultConfig.boot.kernelModules) ipmiModules)
          "default: boot.kernelModules does not contain ipmi_devintf, ipmi_si and ipmi_msghandler"
          ++ lib.optional (!(defaultConfig.users.groups ? ipmi))
          "default: the ipmi group does not exist"
          ++ lib.optional (!lib.elem "ipmi" defaultConfig.users.users.coolerd.extraGroups)
          "default: the service user is not in the ipmi group"
          ++ lib.optional (!lib.all (rule: lib.elem rule (lib.splitString "\n" defaultConfig.services.udev.extraRules)) ipmiUdevRules)
          "default: the udev rules do not give the ipmi group mode 0660 on both ipmi* and ipmidev/*"
          ++ lib.optional (execStartPre hosts.default != [])
          "default: the unit has an ExecStartPre"
          ++ lib.optional (!lib.elem "nct6775" hwmonConfig.boot.kernelModules)
          "hwmon: boot.kernelModules does not contain nct6775"
          ++ lib.optional (!lib.hasInfix "find_chip ${lib.escapeShellArg "nct6798"}\n" accessScript || grantedFiles != ["pwm2" "pwm2_enable" "pwm12" "pwm12_enable"])
          "hwmon: the access helper does not grant exactly pwm2, pwm2_enable, pwm12 and pwm12_enable of nct6798"
          # hwmon-access fails the chip found last; it must be nct6798
          ++ lib.optional (chipsFound twoChipScript != map lib.escapeShellArg ["it8792" "nct6798"] || filesGranted twoChipScript != ["pwm1" "pwm1_enable" "pwm2" "pwm2_enable"])
          "two chips: the access helper does not find it8792 and then nct6798, granting pwm1 and pwm1_enable and then pwm2 and pwm2_enable"
          ++ lib.optional (execStartPre hosts.hwmon != accessCommands accessHelper (hostUser hosts.hwmon))
          "hwmon: ExecStartPre is ${shown (execStartPre hosts.hwmon)}, expected ${shown (accessCommands accessHelper (hostUser hosts.hwmon))}"
          ++ lib.optional (((unit hosts.nvidia).serviceConfig.AmbientCapabilities or []) != [] || lib.hasInfix "AmbientCapabilities" (unitText hosts.nvidia))
          "nvidia: the unit sets AmbientCapabilities"
          # Direct, so a change that drops the capabilities on every host fails
          ++ lib.optional (!lib.all (c: lib.elem c (lib.toList ((unit hosts.drives).serviceConfig.AmbientCapabilities or []))) ["CAP_SYS_RAWIO" "CAP_SYS_ADMIN"])
          "drives: AmbientCapabilities does not contain CAP_SYS_RAWIO and CAP_SYS_ADMIN"
          ++ lib.optional (execStartPre hosts.nvidia != accessCommands accessHelper (hostUser hosts.nvidia))
          "nvidia: ExecStartPre is ${shown (execStartPre hosts.nvidia)}, expected ${shown (accessCommands accessHelper (hostUser hosts.nvidia))}"
          ++ lib.optional (!lib.isDerivation nvidiaCheck || !checksPackage nvidiaCheck pkgs.coolerd)
          "nvidia: system.checks has no coolerd-package-check of pkgs.coolerd"
          # hwmon-access runs accessHelper; both hosts must run that derivation
          ++ lib.optional (execStartPreDrvs hosts.hwmon != [accessHelper.drvPath])
          "hwmon: ExecStartPre does not run the helper derivation that hwmon-access exercises"
          ++ lib.optional (execStartPreDrvs hosts.nvidia != [accessHelper.drvPath])
          "nvidia: ExecStartPre does not run the helper derivation that hwmon-access exercises"
          ++ lib.optional (!rejectsWithoutNvidiaOption nvidiaFanTableZone)
          "nvidia fan as a table without enableNvidiaFans: no failed assertion names services.coolerd.enableNvidiaFans"
          ++ lib.optional (!rejectsWithoutNvidiaOption nvidiaFanPositionalZone)
          "nvidia fan as a list without enableNvidiaFans: no failed assertion names services.coolerd.enableNvidiaFans"
          ++ map (message: "nvidia source without an NVIDIA fan: an assertion fails: ${message}") (failedWithZone nvidiaSourceOnlyZone)
          ++ lib.optional (lib.hasInfix "KERNEL==\"ipmi" hwmonConfig.services.udev.extraRules || lib.elem "ipmi" hwmonConfig.users.users.coolerd.extraGroups || hwmonConfig.users.groups ? ipmi || hasIpmiModule hwmonConfig.boot.kernelModules)
          "hwmon: an IPMI udev rule, ipmi group or IPMI kernel module is present with enableIpmi = false"
          ++ lib.optional (!rejectsBadChip)
          "bad chip: no failed assertion names services.coolerd.hwmon.pwmChannels for the chip nct*"
          ++ lib.optional (migrationConfig.services.coolerd.settings != migrationSettings)
          "migration: services.coolerd.settings is not the settings set on the old path"
          ++ lib.optional (migrationConfig.systemd.services ? ipmi-fan-control || migrationConfig.environment.etc ? "ipmi-fan-control.toml")
          "migration: the unit or config file still has the old name"
          ++ map (leaf: "migration: services.ipmi-fan-control.${lib.concatStringsSep "." leaf} is not an alias") unaliasedLeaves
          ++ map (leaf: "migration: the host does not set services.ipmi-fan-control.${lib.concatStringsSep "." leaf}") unsetOldLeaves
          ++ map (leaf: "post-rename: services.coolerd.${lib.concatStringsSep "." leaf} is not a public option") unknownPostRenameLeaves
          ++ map (name: "migration: ${name} differs from the host that sets the new option paths") migrationDifferences
          # Direct, as a fault common to both twins passes the comparison
          ++ lib.optional ((migrationOld.enableSmartMonTools || migrationOld.enableHdparm) && !lib.elem "disk" migrationConfig.users.users.coolerd.extraGroups)
          "migration: the service user is not in the disk group"
          ++ lib.optional (migrationOld.hwmon.pwmChannels != {} && execStartPre hosts.migration != accessCommands migrationHelper (hostUser hosts.migration))
          "migration: ExecStartPre is ${shown (execStartPre hosts.migration)}, expected ${shown (accessCommands migrationHelper (hostUser hosts.migration))}"
          ++ lib.optional (pkgs.ipmi-fan-control.drvPath != pkgs.coolerd.drvPath)
          "overlay: ipmi-fan-control is not the coolerd derivation"
          ++ lib.optional ((pkgs.coolerd.meta.mainProgram or null) != "coolerd")
          "overlay: meta.mainProgram of coolerd is not coolerd"
          ++ lib.optional (pkgs.coolerd.version != (lib.importTOML ./Cargo.toml).package.version)
          "overlay: the coolerd version is not the version in Cargo.toml"
          ++ lib.optional (self.packages.${system}.ipmi-fan-control.drvPath != self.packages.${system}.default.drvPath)
          "packages: ipmi-fan-control is not the default package"
          ++ lib.optional (!lib.isDerivation defaultCheck || !checksPackage defaultCheck pkgs.coolerd)
          "default: system.checks has no coolerd-package-check of pkgs.coolerd"
          ++ lib.optional ((unit hosts.migration).serviceConfig.ExecStart != "${inertPackage}/bin/coolerd --config /etc/coolerd.toml")
          "migration: ExecStart does not run bin/coolerd of the package set on the old path"
          ++ lib.optional (!lib.isDerivation migrationCheck || !checksPackage migrationCheck inertPackage)
          "migration: system.checks has no coolerd-package-check of the package set on the old path"
          ++ lib.optional (!checksPackage oldNameCheck oldNamePackage)
          "old name: system.checks has no coolerd-package-check of the package";
      in {
        formatter = pkgs.alejandra;

        checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          module-eval = pkgs.runCommand "module-eval" {} ''
            ${lib.concatMapStrings (m: "echo ${lib.escapeShellArg m} >&2\n") moduleFailures}
            ${lib.optionalString (moduleFailures != []) "exit 1"}
            if ! cmp ${migrationConfig.environment.etc."coolerd.toml".source} ${migrationExpected}; then
              echo "migration: /etc/coolerd.toml does not hold the settings set on the old path" >&2
              exit 1
            fi

            # The migration host's check passes once built; a check of another
            # package fails above and is not built, so the Rust package never is
            ${lib.optionalString (lib.isDerivation migrationCheck && checksPackage migrationCheck inertPackage) ": ${migrationCheck}"}

            # The package check scripts of the old-name host and of the nvidia
            # host, which runs as root; they differ only in their package
            old_name_gate=${lib.escapeShellArg oldNameCheck.buildCommand}
            nvidia_gate=${lib.escapeShellArg nvidiaCheck.buildCommand}

            # Runs the package check script $gate as its builder would, against
            # the package directory $1; sets gate_status
            run_gate() {
              rm -f "$TMPDIR/gate-out"
              if package="$1" out="$TMPDIR/gate-out" \
                bash -c "$gate" 2> "$TMPDIR/gate-stderr"; then
                gate_status=0
              else
                gate_status=$?
              fi
            }
            expect_gate_fails() {
              run_gate "$2"
              if [ "$gate_status" -eq 0 ]; then
                echo "$gate_host: $1: the package check passed a package without an executable bin/coolerd file" >&2
                exit 1
              fi
              for expected in services.coolerd.package "$2/bin/coolerd"; do
                if ! grep -q -F -- "$expected" "$TMPDIR/gate-stderr"; then
                  echo "$gate_host: $1: the package check message does not name $expected: $(cat "$TMPDIR/gate-stderr")" >&2
                  exit 1
                fi
              done
            }
            expect_gate_passes() {
              run_gate "$2"
              if [ "$gate_status" -ne 0 ] || [ ! -e "$TMPDIR/gate-out" ]; then
                echo "$gate_host: $1: the package check failed a package with an executable bin/coolerd file: $(cat "$TMPDIR/gate-stderr")" >&2
                exit 1
              fi
            }

            # Package directories for the check; no bin/coolerd in them is run
            fixtures="$TMPDIR/package-fixtures"
            for fixture in directory non-executable executable symlink; do
              mkdir -p "$fixtures/$fixture/bin"
            done
            mkdir "$fixtures/directory/bin/coolerd"
            printf '#!/bin/sh\nexit 1\n' > "$fixtures/non-executable/bin/coolerd"
            chmod 644 "$fixtures/non-executable/bin/coolerd"
            printf '#!/bin/sh\nexit 1\n' > "$fixtures/executable/bin/coolerd"
            chmod 755 "$fixtures/executable/bin/coolerd"
            ln -s "$fixtures/executable/bin/coolerd" "$fixtures/symlink/bin/coolerd"

            # Every case against the package check script of host $1 in $2
            check_gate() {
              gate_host="$1"
              gate="$2"
              expect_gate_fails "old name" ${oldNamePackage}
              expect_gate_fails "directory" "$fixtures/directory"
              expect_gate_fails "non-executable" "$fixtures/non-executable"
              expect_gate_passes "executable" "$fixtures/executable"
              expect_gate_passes "symlink" "$fixtures/symlink"
            }
            check_gate "old name host" "$old_name_gate"
            check_gate "nvidia host" "$nvidia_gate"
            touch $out
          '';

          # Runs the hwmon host's access helper against fake hwmon trees; never /sys
          hwmon-access = pkgs.runCommand "hwmon-access" {} ''
            one_chip_helper=${accessHelper}
            two_chip_helper=${twoChipHelper}
            group=$(id -g)
            failures=0
            umask 022

            fail() {
              echo "case $label: FAIL: $1" >&2
              failures=$((failures + 1))
            }

            start_case() {
              label="$1"
              root="$TMPDIR/$1"
              helper=$one_chip_helper
              failures_before=$failures
              mkdir -p "$root"
            }

            end_case() {
              if [ "$failures" -eq "$failures_before" ]; then
                echo "case $label: pass" >&2
              fi
            }

            make_pwm_files() {
              mkdir -p "$1"
              for file in pwm1 pwm1_enable pwm2 pwm2_enable pwm3 pwm12 pwm12_enable; do
                echo 0 > "$1/$file"
                chmod 644 "$1/$file"
              done
            }

            # A hwmon entry whose name file holds $2 with backslash escapes expanded
            make_chip() {
              make_pwm_files "$1"
              printf '%b' "$2" > "$1/name"
            }

            # As make_chip, but the device directory lies outside the hwmon root
            # and $1 is a symbolic link to it, as in /sys/class/hwmon
            make_linked_chip() {
              device="$TMPDIR/$label-devices/$(basename "$1")/hwmon"
              make_chip "$device" "$2"
              ln -s "$device" "$1"
            }

            run_helper() {
              if "$helper" "$@" 2> "$TMPDIR/stderr"; then
                status=0
              else
                status=$?
              fi
              echo "case $label: exit $status: $(tr '\n' ' ' < "$TMPDIR/stderr")" >&2
            }

            expect_success() {
              if [ "$status" -ne 0 ]; then
                fail "expected exit 0, got $status"
              fi
            }

            expect_failure() {
              if [ "$status" -eq 0 ]; then
                fail "expected a non-zero exit, got 0"
              fi
            }

            expect_stderr() {
              if ! grep -q -F -- "$1" "$TMPDIR/stderr"; then
                fail "stderr does not contain: $1"
              fi
            }

            expect_mode() {
              actual=$(stat -c %a "$1")
              if [ "$actual" != "$2" ]; then
                fail "$1 has mode $actual, expected $2"
              fi
            }

            expect_group() {
              actual=$(stat -c %g "$1")
              if [ "$actual" != "$2" ]; then
                fail "$1 has gid $actual, expected $2"
              fi
            }

            # Every pwm file under $1 still has mode 644
            expect_unchanged() {
              changed=$(find "$1" -type f -name 'pwm*' ! -perm 644)
              if [ -n "$changed" ]; then
                fail "modes changed: $changed"
              fi
            }

            expect_no_other_write() {
              writable=$(find -L "$1" ! -type l -perm -0002)
              if [ -n "$writable" ]; then
                fail "writable by others: $writable"
              fi
            }

            # The mode of every path under the hwmon root, through its symbolic
            # links, except the paths given as arguments
            other_modes() {
              excluded=()
              for target in "$@"; do
                excluded+=(! -path "$target")
              done
              find -L "$root" "''${excluded[@]}" -printf '%p %m\n' | sort
            }

            # The files given as arguments gain group write and the group, and
            # no other path under the hwmon root changes its mode
            expect_grants_to() {
              other_modes "$@" > "$TMPDIR/before"
              run_helper "$root" "$group"
              expect_success
              for target in "$@"; do
                expect_mode "$target" 664
                expect_group "$target" "$group"
              done
              other_modes "$@" > "$TMPDIR/after"
              if ! cmp -s "$TMPDIR/before" "$TMPDIR/after"; then
                fail "other modes changed: $(diff "$TMPDIR/before" "$TMPDIR/after" | tr '\n' ' ')"
              fi
              expect_no_other_write "$root"
            }

            # The four listed files of nct6798 in hwmon0 gain group write, nothing else changes
            expect_grants() {
              expect_grants_to "$root/hwmon0/pwm2" "$root/hwmon0/pwm2_enable" "$root/hwmon0/pwm12" "$root/hwmon0/pwm12_enable"
            }

            start_case a
            make_chip "$root/hwmon0" 'nct6798'
            make_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            start_case b
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'nct6798\n'
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case c-no-match
            make_chip "$root/hwmon0" 'k10temp\n'
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case c-empty
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            end_case

            start_case d
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm12_enable"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm12_enable:"
            expect_mode "$root/hwmon0/pwm2" 644
            expect_unchanged "$root"
            end_case

            start_case e
            make_chip "$root/hwmon0" 'nct6798\n'
            echo 0 > "$TMPDIR/e-outside"
            chmod 644 "$TMPDIR/e-outside"
            rm "$root/hwmon0/pwm2"
            ln -s "$TMPDIR/e-outside" "$root/hwmon0/pwm2"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm2:"
            expect_mode "$TMPDIR/e-outside" 644
            expect_unchanged "$root"
            end_case

            start_case f
            make_chip "$root/hwmon0" 'nct6798\n'
            rm "$root/hwmon0/pwm2"
            mkdir -m 755 "$root/hwmon0/pwm2"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm2:"
            expect_mode "$root/hwmon0/pwm2" 755
            expect_unchanged "$root"
            end_case

            start_case g
            if [ "$(id -u)" -eq 0 ]; then
              echo "case g: skipped, the build runs as root and can read a mode 000 file" >&2
            else
              make_chip "$root/hwmon0" 'nct6798\n'
              make_chip "$root/hwmon1" 'k10temp\n'
              chmod 000 "$root/hwmon1/name"
              run_helper "$root" "$group"
              expect_failure
              expect_stderr "$root/hwmon1/name: cannot read"
              expect_unchanged "$root"
              end_case
            fi

            start_case h
            make_chip "$root/hwmon0" 'nct6798'
            make_chip "$root/hwmon1" 'k10temp\n'
            make_pwm_files "$root/hwmon2"
            expect_grants
            expect_unchanged "$root/hwmon2"
            end_case

            start_case i-trimmed-name
            make_chip "$root/hwmon0" ' \tnct6798\t \n'
            make_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            start_case j-multi-line-name
            make_chip "$root/hwmon0" 'nct6798\nnct6798\n'
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "hwmon chip nct6798:"
            expect_unchanged "$root"
            end_case

            start_case u-usage
            make_chip "$root/hwmon0" 'nct6798\n'
            run_helper
            if [ "$status" -ne 2 ]; then
              fail "no arguments: expected exit 2, got $status"
            fi
            expect_stderr "usage:"
            run_helper "$root" "$group" extra
            if [ "$status" -ne 2 ]; then
              fail "three arguments: expected exit 2, got $status"
            fi
            expect_stderr "usage:"
            expect_unchanged "$root"
            end_case

            start_case k-unknown-group
            make_chip "$root/hwmon0" 'nct6798\n'
            run_helper "$root" no-such-group-coolerd-check
            expect_failure
            expect_stderr "no-such-group-coolerd-check"
            expect_unchanged "$root"
            end_case

            start_case n-class-symlinks
            make_linked_chip "$root/hwmon0" 'nct6798\n'
            make_linked_chip "$root/hwmon1" 'k10temp\n'
            expect_grants
            end_case

            # A name that cannot be read even by root
            start_case o-name-directory
            make_chip "$root/hwmon0" 'nct6798\n'
            make_pwm_files "$root/hwmon1"
            mkdir "$root/hwmon1/name"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon1/name: cannot read"
            expect_unchanged "$root"
            end_case

            start_case l-two-chips
            helper=$two_chip_helper
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'it8792\n'
            expect_grants_to "$root/hwmon1/pwm1" "$root/hwmon1/pwm1_enable" "$root/hwmon0/pwm2" "$root/hwmon0/pwm2_enable"
            end_case

            # nct6798, the chip the helper handles last, fails after it8792 has passed
            start_case m-two-chips-late-failure
            helper=$two_chip_helper
            make_chip "$root/hwmon0" 'nct6798\n'
            make_chip "$root/hwmon1" 'it8792\n'
            rm "$root/hwmon0/pwm2_enable"
            run_helper "$root" "$group"
            expect_failure
            expect_stderr "$root/hwmon0/pwm2_enable:"
            expect_mode "$root/hwmon1/pwm1" 644
            expect_mode "$root/hwmon1/pwm1_enable" 644
            expect_unchanged "$root"
            end_case

            if [ "$failures" -ne 0 ]; then
              echo "$failures failed assertions" >&2
              exit 1
            fi
            touch $out
          '';
        };

        packages = {
          default = pkgs.coolerd;
          coolerd = pkgs.coolerd;
          ipmi-fan-control = pkgs.ipmi-fan-control;
        };

        devShells.default = pkgs.mkShell rec {
          name = "coolerd";
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs = with pkgs; [
            clang
            llvmPackages.bintools
            cargo
            rustc
            clippy
            rustfmt
            freeipmi
          ];

          # https://github.com/rust-lang/rust-bindgen#environment-variables
          LIBCLANG_PATH = pkgs.lib.makeLibraryPath [pkgs.llvmPackages_latest.libclang.lib];

          # Add precompiled library to rustc search path
          RUSTFLAGS = builtins.map (a: ''-L ${a}/lib'') [
            # add libraries here (e.g. pkgs.libvmi)
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs ++ nativeBuildInputs);

          # Add glibc, clang, glib, and other headers to bindgen search path
          BINDGEN_EXTRA_CLANG_ARGS =
            # Includes normal include path
            (builtins.map (a: ''-I"${a}/include"'') [
              # add dev libraries here (e.g. pkgs.libvmi.dev)
              pkgs.glibc.dev
            ])
            # Includes with special directory paths
            ++ [
              ''-I"${pkgs.llvmPackages_latest.libclang.lib}/lib/clang/${pkgs.llvmPackages_latest.libclang.version}/include"''
              ''-I"${pkgs.glib.dev}/include/glib-2.0"''
              ''-I${pkgs.glib.out}/lib/glib-2.0/include/''
            ];
        };
      }
    )
    // {
      # Import the NixOS module from a separate file
      nixosModules.default = import ./nix/module.nix;

      # Import the overlay from a separate file
      overlays.default = import ./nix/overlay.nix;
    };
}
