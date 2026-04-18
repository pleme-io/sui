;; 01_attrs_scaling.lisp — how does NixAttrs lookup time scale with
;; attrset size? The FxHashMap backing NixAttrs is O(1) in theory,
;; but cache misses + hash collisions mean large-N constant factor
;; differs from small-N. This experiment quantifies it.

(defperfexp attrs-point-access-scaling
  :hypothesis
    "NixAttrs.get_sym is ~constant in attrset size after construction.
     The table below should show near-flat median µs across 5/50/500
     keys for a single point access. Construction time DOES grow with
     N (O(n) inserts), and because eval includes construction, we
     expect the absolute numbers to climb — but the eval_expr counter
     should track: big sets with one access do most work on parse +
     build, not on the access itself."
  :variants (
    (:name "attrs-5-access-mid"
      :source "let a = { k0 = 0; k1 = 1; k2 = 2; k3 = 3; k4 = 4; }; in a.k2")
    (:name "attrs-20-access-mid"
      :source "let a = { k00=0; k01=1; k02=2; k03=3; k04=4; k05=5; k06=6; k07=7; k08=8; k09=9; k10=10; k11=11; k12=12; k13=13; k14=14; k15=15; k16=16; k17=17; k18=18; k19=19; }; in a.k10")
    (:name "attrs-50-access-mid"
      :source "let a = { k00=0; k01=1; k02=2; k03=3; k04=4; k05=5; k06=6; k07=7; k08=8; k09=9; k10=10; k11=11; k12=12; k13=13; k14=14; k15=15; k16=16; k17=17; k18=18; k19=19; k20=20; k21=21; k22=22; k23=23; k24=24; k25=25; k26=26; k27=27; k28=28; k29=29; k30=30; k31=31; k32=32; k33=33; k34=34; k35=35; k36=36; k37=37; k38=38; k39=39; k40=40; k41=41; k42=42; k43=43; k44=44; k45=45; k46=46; k47=47; k48=48; k49=49; }; in a.k25"))
  :iterations 500
  :tags ("attrs" "scaling"))

(defperfexp overlay-depth-scaling
  :hypothesis
    "After the overlay-cache fast-path lands (committed in an earlier
     session), repeated get_sym over a deep overlay chain should be
     O(1) once any iter() populates the cache. This experiment feeds
     the chain through `builtins.attrNames` (populates cache) then
     does a series of point accesses. If cache warms correctly the
     variants differ only by construction cost, not access cost."
  :variants (
    (:name "overlay-2-warm-then-access"
      :source
        "let merged = { a = 1; } // { b = 2; };
         in (builtins.length (builtins.attrNames merged)) + merged.a")
    (:name "overlay-4-warm-then-access"
      :source
        "let merged = { a = 1; } // { b = 2; } // { c = 3; } // { d = 4; };
         in (builtins.length (builtins.attrNames merged)) + merged.a")
    (:name "overlay-8-warm-then-access"
      :source
        "let merged = { a = 1; } // { b = 2; } // { c = 3; } // { d = 4; }
                      // { e = 5; } // { f = 6; } // { g = 7; } // { h = 8; };
         in (builtins.length (builtins.attrNames merged)) + merged.a"))
  :iterations 500
  :tags ("attrs" "overlay" "scaling"))
