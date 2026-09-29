;; Complex Soroban-style DeFi contract fixture.
;;
;; Shared by the RPC gateway cache tests and benchmarks so the measured
;; simulation exercises realistic control flow: fee math, constant-product
;; swaps, multi-hop routing, compounding loops, and contract-local state in
;; linear memory (the surface used to prove instance isolation).

(module
  (memory (export "memory") 1)
  (global $fee_bps (mut i32) (i32.const 30))

  ;; amount remaining after a basis-point fee
  (func $after_fee (export "after_fee") (param $amount i32) (param $bps i32) (result i32)
    (i32.sub
      (local.get $amount)
      (i32.div_u (i32.mul (local.get $amount) (local.get $bps)) (i32.const 10000))))

  ;; constant-product output: (in * reserve_out) / (reserve_in + in)
  (func $swap_out (export "swap_out")
    (param $in i32) (param $reserve_in i32) (param $reserve_out i32) (result i32)
    (i32.div_u
      (i32.mul (local.get $in) (local.get $reserve_out))
      (i32.add (local.get $reserve_in) (local.get $in))))

  ;; compound a principal for $steps periods at $bps per period
  (func $compound (export "compound")
    (param $principal i32) (param $steps i32) (param $bps i32) (result i32)
    (local $value i32)
    (local $i i32)
    (local.set $value (local.get $principal))
    (block $done
      (loop $step
        (br_if $done (i32.ge_s (local.get $i) (local.get $steps)))
        (local.set $value
          (i32.add
            (local.get $value)
            (i32.div_u (i32.mul (local.get $value) (local.get $bps)) (i32.const 10000))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $step)))
    (local.get $value))

  ;; multi-hop routing: charge the fee once, then swap across $hops pools
  (func $route (export "route")
    (param $amount i32) (param $hops i32) (param $reserve_in i32) (param $reserve_out i32) (result i32)
    (local $amount_out i32)
    (local $i i32)
    (local.set $amount_out (call $after_fee (local.get $amount) (global.get $fee_bps)))
    (block $done
      (loop $step
        (br_if $done (i32.ge_s (local.get $i) (local.get $hops)))
        (local.set $amount_out
          (call $swap_out
            (local.get $amount_out)
            (local.get $reserve_in)
            (local.get $reserve_out)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $step)))
    (local.get $amount_out))

  ;; price impact in basis points for a given input amount
  (func $price_impact (export "price_impact")
    (param $amount i32) (param $reserve_in i32) (param $reserve_out i32) (result i32)
    (local $out i32)
    (local.set $out
      (call $swap_out (local.get $amount) (local.get $reserve_in) (local.get $reserve_out)))
    (i32.div_u
      (i32.mul (i32.sub (local.get $reserve_out) (local.get $out)) (i32.const 10000))
      (local.get $reserve_out)))

  ;; entrypoint used by simulateTransaction benchmarks
  (func $simulate (export "simulate")
    (param $amount i32) (param $reserve_in i32) (param $reserve_out i32) (param $hops i32) (result i32)
    (call $route
      (local.get $amount)
      (local.get $hops)
      (local.get $reserve_in)
      (local.get $reserve_out)))

  ;; persist a settlement into contract-local linear memory
  (func $settle (export "settle") (param $slot i32) (param $value i32)
    (i32.store (local.get $slot) (local.get $value)))

  (func $read (export "read") (param $slot i32) (result i32)
    (i32.load (local.get $slot)))

  ;; integer hash mix, standing in for per-contract bookkeeping math
  (func $mix (export "mix") (param $seed i32) (result i32)
    (local $x i32)
    (local.set $x (local.get $seed))
    (local.set $x (i32.xor (local.get $x) (i32.shr_u (local.get $x) (i32.const 16))))
    (local.set $x (i32.mul (local.get $x) (i32.const 0x45d9f3b)))
    (local.set $x (i32.xor (local.get $x) (i32.shr_u (local.get $x) (i32.const 16))))
    (local.get $x))
)
