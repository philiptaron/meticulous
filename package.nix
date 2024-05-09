{
  lib,
  stdenv,
  craneLib,
  binaryen,
  mdbook,
  pkg-config,
  protobuf,
  llvmPackages,
  openssl,
  libiconv,
  python3,
  python311Packages,
  mypy,
  black,
  version ? null
}:

let
  inherit (craneLib) buildPackage filterCargoSources path;
  inherit (lib) cleanSourceWith optionals;
  inherit (lib.strings) match;

  # Only keeps `.tar` and `.proto` files, plus normal Rust files.
  srcFilter = path: type: (match ".*\.(tar|proto)$" path != null) || (filterCargoSources path type);

  self = buildPackage {
    pname = "maelstrom";
    inherit version;

    src = cleanSourceWith {
      src = path ./.;
      filter = srcFilter;
    };

    strictDeps = true;

    # maelstrom-web has a WASM build step that shells out to `cargo`, and needs to be informed about
    # being inside the Nix build sandbox.
    postUnpack = ''
      mkdir source/crates/maelstrom-web/.cargo
      ln -s ${self.cargoVendorDir}/config.toml source/crates/maelstrom-web/.cargo/config.toml
    '';

    nativeBuildInputs = [
      binaryen
      black
      pkg-config
      llvmPackages.bintools
      mypy
      protobuf
      python3
      python311Packages.grpcio-tools
      python311Packages.pytest
      python311Packages.types-protobuf
      python311Packages.xdg-base-dirs
      mdbook
    ];

    buildInputs = [ openssl ] ++ optionals stdenv.isDarwin [ libiconv ];

    # Don't run the unit tests inside Nix build.
    doCheck = false;
  };
in
self
