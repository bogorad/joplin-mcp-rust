{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.joplin-mcp;
  toml = pkgs.formats.toml { };
  serviceName = "joplin-mcpd";
  credentialsDir = "/run/credentials/${serviceName}.service";
  credentialFiles = {
    postgres-joplin-dsn = "${credentialsDir}/postgres-joplin-dsn";
    postgres-mcp-dsn = "${credentialsDir}/postgres-mcp-dsn";
    token-hmac-key = "${credentialsDir}/token-hmac-key";
  };
  baseSettings = {
    postgres = {
      joplin_dsn_file = credentialFiles.postgres-joplin-dsn;
      mcp_dsn_file = credentialFiles.postgres-mcp-dsn;
      runtime_max_connections = 12;
      indexer_max_connections = 4;
      acquire_timeout_seconds = 5;
      statement_timeout_seconds = 15;
    };
    tokens = {
      active_hmac_key_id = "primary";
      hmac_keys = [
        {
          id = "primary";
          file = credentialFiles.token-hmac-key;
        }
      ];
    };
    server = {
      listen = "127.0.0.1:8081";
      public_base_url = "https://joplin-mcp.lan";
      tls_mode = "required";
      allowed_origins = [ "https://joplin-mcp.lan" ];
      allow_insecure_localhost = false;
      lan_cidrs = [ ];
      trusted_proxies = [ ];
    };
  };
  configFile = toml.generate "joplin-mcpd.toml" (lib.recursiveUpdate baseSettings cfg.settings);
in
{
  options.services.joplin-mcp = {
    enable = lib.mkEnableOption "Joplin MCP service";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "Package providing the joplin-mcpd executable.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "joplin-mcp";
      description = "System user that runs joplin-mcpd.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "joplin-mcp";
      description = "System group that runs joplin-mcpd.";
    };

    credentials = {
      postgresJoplinDsn = lib.mkOption {
        type = lib.types.path;
        description = "Host path loaded as the postgres-joplin-dsn systemd credential.";
      };

      postgresMcpDsn = lib.mkOption {
        type = lib.types.path;
        description = "Host path loaded as the postgres-mcp-dsn systemd credential.";
      };

      tokenHmacKey = lib.mkOption {
        type = lib.types.path;
        description = "Host path loaded as the token-hmac-key systemd credential.";
      };
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = "Non-secret joplin-mcpd TOML settings.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.package != null;
        message = "services.joplin-mcp.package must be set.";
      }
    ];

    users.groups.${cfg.group} = { };
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = "/var/lib/joplin-mcp";
    };

    systemd.services.${serviceName} = {
      description = "Joplin MCP server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment.JP_MCPD_CONFIG = "${configFile}";
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/joplin-mcpd --config ${configFile}";
        User = cfg.user;
        Group = cfg.group;
        DynamicUser = false;
        StateDirectory = "joplin-mcp";
        RuntimeDirectory = "joplin-mcp";
        UMask = "0077";
        LoadCredential = [
          "postgres-joplin-dsn:${cfg.credentials.postgresJoplinDsn}"
          "postgres-mcp-dsn:${cfg.credentials.postgresMcpDsn}"
          "token-hmac-key:${cfg.credentials.tokenHmacKey}"
        ];
      };
    };
  };
}
