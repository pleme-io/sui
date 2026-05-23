;; sui-spec/specs/gc.lisp — typed border for the nix store garbage
;; collector.  Today's algorithms cover the cppnix stop-the-world
;; baseline; future entries may include concurrent / lazy variants.

;; ── cppnix stop-the-world (the baseline) ──────────────────────────

(defgc-algorithm
  :name "cppnix-stop-the-world"
  :phases ((:kind LockStore)
           (:kind CollectGcRoots      :bind "roots")
           (:kind ScanStore                              :bind "all-paths")
           (:kind ComputeLiveSet      :from "roots"      :bind "live")
           (:kind ComputeDeadSet      :from "all-paths"  :bind "dead")
           (:kind FilterByAgeAndSize  :from "dead"       :bind "to-delete")
           (:kind DeleteDeadPaths     :from "to-delete")
           (:kind UnlockStore)
           (:kind EmitReport)))

;; ── attested stop-the-world (homelab default) ────────────────────
;;
;; Identical to stop-the-world but appends an AttestRunToChain phase
;; so the deletion event lands on the OutcomeChain audit log.  Hosts
;; without the chain skip this algorithm via SubstituterTrustLevel
;; or via the planner picking the unattested variant.

(defgc-algorithm
  :name "cppnix-stop-the-world-attested"
  :phases ((:kind LockStore)
           (:kind CollectGcRoots      :bind "roots")
           (:kind ScanStore                              :bind "all-paths")
           (:kind ComputeLiveSet      :from "roots"      :bind "live")
           (:kind ComputeDeadSet      :from "all-paths"  :bind "dead")
           (:kind FilterByAgeAndSize  :from "dead"       :bind "to-delete")
           (:kind DeleteDeadPaths     :from "to-delete")
           (:kind UnlockStore)
           (:kind AttestRunToChain    :from "to-delete")
           (:kind EmitReport)))
