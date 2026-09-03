{
  description = "Minimal image viewer for the Kitty graphics protocol";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          mkPackage =
            {
              pname,
              feature,
              optLevel,
              description,
            }:
            pkgs.pkgsStatic.rustPlatform.buildRustPackage {
              inherit pname;
              version = "0.1.0";
              src = pkgs.lib.cleanSource ./.;
              cargoLock.lockFile = ./Cargo.lock;
              buildNoDefaultFeatures = true;
              buildFeatures = [ feature ];
              env.CARGO_PROFILE_RELEASE_OPT_LEVEL = optLevel;
              meta = {
                inherit description;
                mainProgram = "kitkat-rs";
              };
            };
        in
        rec {
          default = quality;
          quality = mkPackage {
            pname = "kitkat-rs";
            feature = "quality";
            optLevel = "z";
            description = "Lanczos image viewer for terminals implementing the Kitty graphics protocol";
          };
          "low-rss" = mkPackage {
            pname = "kitkat-rs-low-rss";
            feature = "low-rss";
            optLevel = "3";
            description = "Low-memory streaming PNG viewer using nearest-neighbor downscaling";
          };
          faster = mkPackage {
            pname = "kitkat-rs-faster";
            feature = "faster";
            optLevel = "3";
            description = "Parallel Lanczos image viewer for fast, high-quality downscaling";
          };
          fastest = mkPackage {
            pname = "kitkat-rs-fastest";
            feature = "fastest";
            optLevel = "3";
            description = "Maximum-throughput image viewer using parallel nearest-neighbor downscaling";
          };
        }
      );
    };
}
