;; sui-spec/specs/dockerfile.lisp — canonical Dockerfile->graph spec.
;;
;; A `(defdockerfile-graph ...)` form names one canonical Dockerfile
;; instance the interpreter is proven against.  The interpreter
;; (sui_spec::dockerfile::apply) parses the referenced Dockerfile
;; text (via the injectable Environment trait) and builds a
;; content-addressed graph of typed instruction nodes -- each node's
;; hash is a function of (instruction-kind, prior-layer-digest,
;; resolved-build-arg-values), so identical layers hash identically
;; run over run, mirroring Nix derivation hashing.
;;
;; Two canonical instances:
;;   1. "simple-three-instruction" -- FROM/RUN/CMD, no build args.
;;   2. "nonroot-gateway" -- approximates a real production-style
;;      nonroot gateway image: apt-get installs, a
;;      `RUN --mount=type=bind,source=.,target=/build` FIPS install
;;      block gated on ARG FIPS + ARG TARGETARCH, and a final
;;      adduser chain.

(defdockerfile-graph
  :name          "simple-three-instruction"
  :description   "Minimal FROM/RUN/CMD Dockerfile -- no build args, no mounts."
  :source-text   "FROM debian:bookworm-slim\nRUN apt-get update && apt-get install -y ca-certificates\nCMD [\"/bin/true\"]\n")

(defdockerfile-graph
  :name          "nonroot-gateway"
  :description   "Approximates Dockerfile.nonroot_gateway -- apt-get installs, a RUN --mount=type=bind FIPS block gated on ARG FIPS + ARG TARGETARCH, and an adduser chain."
  :source-text   "FROM debian:bookworm-slim\nARG TARGETARCH\nARG FIPS=false\nRUN apt-get update && apt-get install -y ca-certificates curl\nRUN --mount=type=bind,source=.,target=/build echo installing FIPS for $TARGETARCH if requested\nENV GATEWAY_HOME=/opt/gateway\nWORKDIR /opt/gateway\nCOPY --from=build /build/target/release/gateway /opt/gateway/gateway\nRUN adduser --disabled-password --gecos '' --home /opt/gateway gateway && chown gateway:gateway /opt/gateway/gateway\nENTRYPOINT [\"/opt/gateway/gateway\"]\nCMD [\"--config\", \"/opt/gateway/config.yaml\"]\n")
