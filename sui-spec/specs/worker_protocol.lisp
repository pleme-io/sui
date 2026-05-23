;; sui-spec/specs/worker_protocol.lisp — typed border for the
;; nix-daemon worker protocol.  Versioned envelope + one form per
;; opcode.  Reference: cppnix libstore/worker-protocol.cc.

;; ── Protocol envelope ────────────────────────────────────────────

(defworker-protocol
  :name "cppnix-worker-protocol"
  :version 35
  :magic-client "0x6e697863"   ;; "nixc"
  :magic-server "0x6478696f")  ;; "dxio"

;; ── Opcodes (~30 covering the full r/w surface) ──────────────────

(defworker-opcode
  :name           "IsValidPath"
  :code           1
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (Bool)
  :since-version  1)

(defworker-opcode
  :name           "QueryReferrers"
  :code           7
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (StorePathList)
  :since-version  1)

(defworker-opcode
  :name           "AddToStore"
  :code           9
  :direction      ClientToDaemon
  :request-fields (Str Str StorePathList Bytes)
  :response-fields (ValidPathInfo)
  :since-version  25)

(defworker-opcode
  :name           "BuildPaths"
  :code           10
  :direction      ClientToDaemon
  :request-fields (StorePathList BuildMode)
  :since-version  1)

(defworker-opcode
  :name           "EnsurePath"
  :code           11
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :since-version  1)

(defworker-opcode
  :name           "AddTempRoot"
  :code           12
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :since-version  1)

(defworker-opcode
  :name           "AddIndirectRoot"
  :code           13
  :direction      ClientToDaemon
  :request-fields (Str)
  :since-version  1)

(defworker-opcode
  :name           "SyncWithGC"
  :code           14
  :direction      ClientToDaemon
  :since-version  1)

(defworker-opcode
  :name           "FindRoots"
  :code           15
  :direction      ClientToDaemon
  :response-fields (KeyValueAttrs)
  :since-version  1)

(defworker-opcode
  :name           "SetOptions"
  :code           19
  :direction      ClientToDaemon
  :request-fields (KeyValueAttrs)
  :since-version  1)

(defworker-opcode
  :name           "CollectGarbage"
  :code           20
  :direction      ClientToDaemon
  :request-fields (U64 U64 StorePathList U64)
  :response-fields (StorePathList U64 U64)
  :since-version  1)

(defworker-opcode
  :name           "QueryAllValidPaths"
  :code           23
  :direction      ClientToDaemon
  :response-fields (StorePathList)
  :since-version  1)

(defworker-opcode
  :name           "QueryPathInfo"
  :code           26
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (ValidPathInfo)
  :since-version  17)

(defworker-opcode
  :name           "QueryDerivationOutputNames"
  :code           28
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (StringList)
  :since-version  1)

(defworker-opcode
  :name           "QueryPathFromHashPart"
  :code           29
  :direction      ClientToDaemon
  :request-fields (Str)
  :response-fields (StorePath)
  :since-version  1)

(defworker-opcode
  :name           "QuerySubstitutablePathInfos"
  :code           30
  :direction      ClientToDaemon
  :request-fields (StorePathList)
  :response-fields (Substitutables)
  :since-version  3)

(defworker-opcode
  :name           "QueryValidPaths"
  :code           31
  :direction      ClientToDaemon
  :request-fields (StorePathList Bool)
  :response-fields (StorePathList)
  :since-version  12)

(defworker-opcode
  :name           "QuerySubstitutablePaths"
  :code           32
  :direction      ClientToDaemon
  :request-fields (StorePathList)
  :response-fields (StorePathList)
  :since-version  12)

(defworker-opcode
  :name           "QueryValidDerivers"
  :code           33
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (StorePathList)
  :since-version  1)

(defworker-opcode
  :name           "OptimiseStore"
  :code           34
  :direction      ClientToDaemon
  :since-version  14)

(defworker-opcode
  :name           "VerifyStore"
  :code           35
  :direction      ClientToDaemon
  :request-fields (Bool Bool)
  :response-fields (Bool)
  :since-version  1)

(defworker-opcode
  :name           "BuildDerivation"
  :code           36
  :direction      ClientToDaemon
  :request-fields (StorePath Bytes BuildMode)
  :response-fields (KeyedBuildResult)
  :since-version  14)

(defworker-opcode
  :name           "AddSignatures"
  :code           37
  :direction      ClientToDaemon
  :request-fields (StorePath StringList)
  :since-version  18)

(defworker-opcode
  :name           "NarFromPath"
  :code           38
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (Bytes)
  :since-version  19)

(defworker-opcode
  :name           "AddToStoreNar"
  :code           39
  :direction      ClientToDaemon
  :request-fields (ValidPathInfo Bytes Bool Bool)
  :since-version  17)

(defworker-opcode
  :name           "QueryMissing"
  :code           40
  :direction      ClientToDaemon
  :request-fields (StorePathList)
  :response-fields (StorePathList StorePathList StorePathList U64 U64)
  :since-version  19)

(defworker-opcode
  :name           "QueryDerivationOutputMap"
  :code           41
  :direction      ClientToDaemon
  :request-fields (StorePath)
  :response-fields (DerivationOutputs)
  :since-version  22)

(defworker-opcode
  :name           "RegisterDrvOutput"
  :code           42
  :direction      ClientToDaemon
  :request-fields (Str Str)
  :since-version  31)

(defworker-opcode
  :name           "QueryRealisation"
  :code           43
  :direction      ClientToDaemon
  :request-fields (Str)
  :response-fields (RealisationsMap)
  :since-version  31)

(defworker-opcode
  :name           "AddMultipleToStore"
  :code           44
  :direction      ClientToDaemon
  :request-fields (Bool Bool Bytes)
  :since-version  32)

(defworker-opcode
  :name           "AddBuildLog"
  :code           45
  :direction      ClientToDaemon
  :request-fields (StorePath Bytes)
  :since-version  32)

(defworker-opcode
  :name           "BuildPathsWithResults"
  :code           46
  :direction      ClientToDaemon
  :request-fields (StorePathList BuildMode)
  :response-fields (KeyedBuildResult)
  :since-version  34)

(defworker-opcode
  :name           "AddPermRoot"
  :code           47
  :direction      ClientToDaemon
  :request-fields (StorePath Str Bool)
  :response-fields (Str)
  :since-version  1)
