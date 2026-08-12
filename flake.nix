{
  description = "Kanshou (監視) — typed introspection over a Unix socket: an Introspect trait, a derive, and a non-fatal sidecar every fleet binary can expose";

  inputs.substrate.url = "github:pleme-io/substrate";

  outputs =
    { substrate, ... }:
    substrate.rust.workspace {
      src = ./.;
      # Two members: `kanshou` (the library every consumer imports) and
      # `kanshou-derive` (its proc-macro half, pulled in transitively). The
      # builder needs one named member and this is the one consumers name.
      member = "kanshou";
    };
}
