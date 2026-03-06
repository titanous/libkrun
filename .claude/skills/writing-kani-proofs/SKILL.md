---
name: writing-kani-proofs
description: Use when writing Kani bounded model checking proofs, adding verification harnesses to Rust code, or reviewing existing Kani proofs for correctness - enforces production coupling, non-tautological design, coverage discipline, and verified token patterns
user-invocable: true
---

# Writing Kani Proofs

Kani proofs are bounded model checking harnesses that verify properties of Rust code exhaustively within a bounded state space. Unlike tests (which check examples), proofs check ALL inputs up to the bound.

## When to Use

- Adding `#[cfg(kani)] mod verification` blocks to production code
- Reviewing existing proofs for tautology or production-coupling gaps
- Verifying unsafe code, bit manipulation, serialization round-trips, bounds checks
- Creating verified token types that enforce invariants in both proofs and production

## Core Principle: Proofs Must Break When Code Breaks

A proof that passes regardless of the implementation is worthless. Every proof must be tied to production code via one or more coupling mechanisms such that if the invariant being tested is violated in production code, the proof fails.

**Before writing any proof, answer: "What production code change would make this proof fail?"** If you cannot answer concretely, redesign the proof.

## Before Writing Any Proof

1. **Read the production source file.** Identify the exact function signatures, field names, and types. Do not guess from memory or a description — read the actual code.
2. **Identify the API surface.** Only call functions that exist. Never invent methods (`mark_range`, `drain`, etc.) that aren't in the source.
3. **Check for existing proofs.** The file may already have a `#[cfg(kani)] mod verification` block. Append to it; do not create a duplicate module.

## Proof Structure Convention

All proofs live inline as `#[cfg(kani)] mod verification` at the bottom of the source file they verify:

```rust
#[cfg(kani)]
mod verification {
    use super::*;

    /// [One-line property statement]
    ///
    /// [Why this matters for production code]
    /// [What production change would break this proof]
    ///
    /// Bound: [unwind rationale]
    #[kani::proof]
    #[kani::unwind(N)]
    fn proof_descriptive_name() {
        // 1. Symbolic inputs
        let x: Type = kani::any_where(|&v| precondition(v));

        // 2. Exercise production code
        let result = production_function(x);

        // 3. Assert property
        kani::assert(property(result), "message");

        // 4. Coverage: verify proof exercises intended paths
        kani::cover!(interesting_condition, "description");
    }
}
```

## Production Coupling Mechanisms

Use at least one of these to tie each proof to production code. The best proofs use multiple.

### 1. Direct Function Call

The proof calls the actual production function and asserts on its output. Changing the function changes the proof result.

```rust
// GOOD: calls production mark_dirty, asserts on production is_dirty
bitmap.mark_dirty(addr);
kani::assert(bitmap.is_dirty(page_idx), "mark must be visible via is_dirty");

// BAD: reimplements the logic instead of calling production code
let word_idx = pfn / 64;
let bit = 1u64 << (pfn % 64);
kani::assert(words[word_idx] & bit != 0, "bit is set");
// ^ This proves your reimplementation, not the production code
```

### 2. Round-Trip Verification

Encode then decode (or write then read) must be identity. Changing either half breaks the proof.

```rust
// Proves write_le_u32 and read_le_u32 are inverses
let val: u32 = kani::any();
let mut buf = [0u8; 4];
write_le_u32(&mut buf, val);
kani::assert(read_le_u32(&buf) == val, "LE u32 round-trip");
```

### 3. Contracts (`#[kani::requires]` / `#[kani::ensures]`)

Attach preconditions and postconditions directly to production functions. The contract IS the production code — changing the function signature or behavior breaks the contract.

```rust
// On the production function:
#[cfg_attr(kani, kani::requires(limit <= 0xFFFFF))]
#[cfg_attr(kani, kani::ensures(|&result| get_base(result) == u64::from(base)))]
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 { ... }

// In the verification module:
#[kani::proof_for_contract(gdt_entry)]
fn proof_contract_gdt_entry() {
    let flags: u16 = kani::any();
    let base: u32 = kani::any();
    let limit: u32 = kani::any();
    gdt_entry(flags, base, limit);
}
```

A `#[kani::proof_for_contract(f)]` harness requires that `f` has `requires`/`ensures` annotations. Do not use `proof_for_contract` without first adding the contract to the production function.

### 4. Compositional Verification (`#[kani::stub_verified]`)

Once a function's contract is proven, use it as a trusted abstraction in proofs of callers:

```rust
#[kani::proof]
#[kani::stub_verified(gdt_entry)]
fn proof_kvm_segment_selector() {
    // gdt_entry calls are replaced with its verified contract
    let seg = kvm_segment_from_gdt(entry, table_index);
    kani::assert(seg.selector == table_index as u16 * 8, "selector encoding");
}
```

### 5. Verified Token Types

Create a newtype that can only be constructed through a bounds-checking constructor, then use it in both production code and proofs. The type system enforces the invariant.

```rust
// Production code:
pub(crate) struct ValidatedHostPtr<'a> {
    ptr: *const u8,
    region_start: u64,
    region_len: u64,
    addr: u64,
    _marker: PhantomData<&'a u8>,
}

// Can ONLY be constructed by get_validated_host_ptr, which verifies bounds.
// Production callers use ValidatedHostPtr::as_slice() instead of raw slice::from_raw_parts.
// The proof verifies the constructor; the type system propagates the invariant.
```

The proof verifies the constructor's bounds check, and all production call sites are forced through the validated type. Changing the constructor or removing the check breaks the proof AND removes compile-time enforcement.

### 6. Negative Proofs

Verify that invalid inputs produce errors (not silent corruption):

```rust
// Proves wrong magic is rejected — changing validation logic breaks this
let header = SnapshotHeader { magic: kani::any_where(|&m| m != SNAPSHOT_MAGIC), .. };
let result = validate_magic_and_version(&header);
kani::assert(matches!(result, Err(SnapshotError::InvalidMagic)), "wrong magic rejected");
```

## Avoiding Tautological Proofs

A tautological proof restates the implementation rather than verifying a property.

### Red Flags

| Pattern | Problem | Fix |
|---------|---------|-----|
| Proof reimplements the function logic | Proves your copy, not production | Call the production function directly |
| `kani::assert(f(x) == f(x))` | Always true | Assert a meaningful property of the result |
| Only tests the happy path | Misses error handling | Add negative proofs and boundary cases |
| `kani::cover!(true, "reachable")` | Always satisfiable, proves nothing | Cover specific conditions: `kani::cover!(pfn == 63)` |
| Proof would pass with a no-op stub | Implementation not actually tested | Verify output depends on input |

### The "Sequence Test" Anti-Pattern

Testing `mark → count == 1 → clear → count == 0` on a fresh bitmap does NOT prove count equals popcount. A broken `count()` that returns `marks_called - clears_called` would also pass. Instead, prove a structural property: mark two distinct pages, assert count == 2. Or: mark page A, verify `count() == popcount(word[0]) + popcount(word[1]) + ...` by calling a reference implementation.

### The "Wrong Implementation" Test

Mentally substitute a subtly wrong implementation. Would your proof still pass?

- If `mark()` used `fetch_xor` instead of `fetch_or`: does your idempotency proof catch this? (It should.)
- If `validate_magic` returned `Ok(())` for all inputs: does your proof catch this? (Negative proof should.)
- If `gdt_entry` swapped base and limit fields: does your round-trip proof catch this? (It should.)

## Coverage Annotations

`kani::cover!()` verifies that the proof's symbolic inputs actually reach specific code paths. Without coverage, a proof might pass trivially because `kani::assume()` over-constrains the inputs to an empty set.

### Mandatory Coverage Checks

1. **After any `kani::assume()`**: verify the remaining state space is non-empty
2. **At branch points**: verify both branches are reachable
3. **At boundary values**: verify edge cases are exercised (word boundaries, max values)

```rust
// GOOD: verifies specific conditions the proof should exercise
kani::cover!(pfn == 63, "last bit of word 0");
kani::cover!(pfn == 64, "first bit of word 1");
kani::cover!(pfn_a / 64 == pfn_b / 64, "same-word isolation");
kani::cover!(pfn_a / 64 != pfn_b / 64, "cross-word isolation");

// BAD: always satisfiable, proves nothing
kani::cover!(true, "path reachable");

// BAD: trivially true because of preceding code (mark(63) already called)
bitmap.mark(63);
kani::cover!(bitmap.is_set(63), "PFN 63 reachable");  // always true!
// GOOD: instead cover the condition BEFORE the action, or cover an input property
kani::cover!(pfn == 63, "boundary PFN 63 exercised");
```

If a `kani::cover!()` returns UNSATISFIABLE, investigate — your assumptions may have eliminated the case you intended to test.

Coverage annotations should verify that **symbolic inputs** reach interesting regions of the state space, not that deterministic code produces its expected output. Place `cover!` on input conditions or branch-point conditions, not on post-assertion outcomes.

## Kani Feature Reference

### Symbolic Values

| Feature | Syntax | Use |
|---------|--------|-----|
| Unconstrained symbolic | `kani::any::<T>()` | Full type range |
| Constrained symbolic | `kani::any_where(\|&v\| cond)` | Bounded state space |
| Custom generation | `impl kani::Arbitrary for T` | Complex types with invariants |
| Derive generation | `#[cfg_attr(kani, derive(kani::Arbitrary))]` | Simple structs/enums |
| Arbitrary array | `kani::Arbitrary::any_array::<N>()` | Fixed-size `[T; N]` |
| Path constraint | `kani::assume(cond)` | Filter input space |

### Assertions and Coverage

| Feature | Syntax | Use |
|---------|--------|-----|
| Property check | `kani::assert(cond, "msg")` | Must hold on all paths |
| Reachability | `kani::cover!(cond, "desc")` | Verify path is exercisable |
| Expected panic | `#[kani::should_panic]` | Negative test: must panic |

### Harness Attributes

| Attribute | Syntax | Use |
|-----------|--------|-----|
| Proof marker | `#[kani::proof]` | Entry point for verification |
| Loop bound | `#[kani::unwind(N)]` | N = max iterations + 1 |
| Solver | `#[kani::solver(cadical)]` | Pin fastest solver per proof (see solver sweep below) |
| Contract proof | `#[kani::proof_for_contract(f)]` | Verify f's requires/ensures |
| Stub verified | `#[kani::stub_verified(f)]` | Trust f's proven contract |
| Stub replace | `#[kani::stub(orig, replacement)]` | Replace function in proof |

### Function Contracts

| Annotation | Syntax | Use |
|------------|--------|-----|
| Precondition | `#[kani::requires(cond)]` | Caller must guarantee |
| Postcondition | `#[kani::ensures(\|&result\| cond)]` | Function must guarantee |
| Memory effect | `#[kani::modifies(ptr)]` | Declares what is mutated |
| History expr | `old(expr)` in ensures | Compare pre/post state |

### Loop Contracts (Unstable)

| Annotation | Syntax | Use |
|------------|--------|-----|
| Invariant | `#[kani::loop_invariant(cond)]` | Induction hypothesis |
| Loop effect | `#[kani::loop_modifies(ptr)]` | Memory modified in loop |

### Quantifiers

| Feature | Syntax | Use |
|---------|--------|-----|
| Universal | `kani::forall!(\|x\| cond)` | For all x, cond holds |
| Existential | `kani::exists!(\|x\| cond)` | There exists x where cond |

### CLI Options

| Flag | Use |
|------|-----|
| `--harness name` | Run single proof |
| `--default-unwind N` | Global loop bound |
| `-Z function-contracts` | Enable contracts |
| `-Z stubbing` | Enable stubs |
| `-Z concrete-playback` | Generate unit tests from counterexamples |
| `--concrete-playback=print` | Print generated test |
| `--concrete-playback=inplace` | Insert test into source |

## Unwind Bound Calculation

The unwind bound must be `max_iterations + 1` for each loop. When a proof has nested loops, the outermost unwind applies to ALL loops.

| Code Pattern | Loop Count | Unwind |
|-------------|-----------|--------|
| `Vec::new(n)` where n <= K | K iterations | K + 1 |
| Byte loop (u16 LE) | 2 | 3 |
| Byte loop (u32 LE) | 4 | 5 |
| Byte loop (u64 LE) | 8 | 9 |
| Bitmap word iteration (N pages) | ceil(N/64) | ceil(N/64) + 1 |
| Inner bit scan (64 bits) | 64 | 65 |
| Combined outer words + inner bits | ceil(N/64) + 64 | ceil(N/64) + 65 |

Always document unwind rationale in the proof's doc comment.

## Solver Sweep

Different solvers have wildly different performance on different proofs (10x+ differences are common). After a proof passes, sweep all solvers on that specific harness and tag it with the fastest:

```bash
for solver in cadical kissat minisat z3; do
  echo "=== $solver ==="
  time cargo kani --harness proof_name -- --solver $solver
done
```

Then add `#[kani::solver(winner)]` to the proof. Omit the attribute only if cadical (the default) wins.

## Advanced Proof Techniques

### Centralized Invariant Helpers

Extract type invariants into an `is_valid()` method reusable across proofs and `Arbitrary` impls. One change updates all proofs:

```rust
impl TokenBucket {
    fn is_valid(&self) -> bool {
        self.size != 0
            && self.refill_time != 0
            && self.budget <= self.size
    }
}

// In Arbitrary:
kani::assume(bucket.is_valid());

// In proofs:
bucket.auto_replenish();
kani::assert(bucket.is_valid(), "invariant preserved after replenish");
```

### Custom Arbitrary with Factory + Invariant

Don't just set fields — construct through production APIs, then fuzz internal state:

```rust
#[cfg(kani)]
impl kani::Arbitrary for TokenBucket {
    fn any() -> TokenBucket {
        // Use production constructor to guarantee structural validity
        let bucket = TokenBucket::new(kani::any(), kani::any(), kani::any());
        kani::assume(bucket.is_some());
        let mut bucket = bucket.unwrap();
        // Fuzz mutable internal state within invariant bounds
        bucket.budget = kani::any();
        kani::assume(bucket.is_valid());
        bucket
    }
}
```

### Simplified Abstraction Layers

When production types have loops (e.g. multi-region memory with binary search), create a single-element variant that eliminates the loop. This can reduce unwind from N+1 to 0, a 10-100x solver speedup:

```rust
/// Single-region memory model — eliminates find_region loop.
pub struct ProofGuestMemory {
    the_region: GuestRegionMmap,
}

impl GuestMemory for ProofGuestMemory {
    fn find_region(&self, addr: GuestAddress) -> Option<&Self::R> {
        // No loop — direct check against single region
        self.the_region.to_region_addr(addr).map(|_| &self.the_region)
    }
}

#[kani::proof]
#[kani::unwind(0)]  // No loops left to unwind
fn proof_add_used() {
    let mem = ProofGuestMemory::new(kani::any());
    // ...
}
```

### Semantic Stubs with State

When stubbing functions that have behavioral contracts (e.g. monotonic time), maintain state in the stub:

```rust
mod stubs {
    static mut LAST_SECONDS: i64 = 0;

    fn instant_now() -> Instant {
        let next = kani::any_where(|n| *n >= unsafe { LAST_SECONDS });
        unsafe { LAST_SECONDS = next; }
        // ... construct Instant from next
    }
}

#[kani::proof]
#[kani::stub(Instant::now, stubs::instant_now)]
fn proof_token_refill_monotonic() { ... }
```

### Intentional Scope Bounds

When full verification is infeasible, explicitly bound the proof scope with a documented justification:

```rust
/// Verify descriptor chain processing for chains up to length 4.
/// Production max is 256, but 4 covers: empty, single, boundary (power-of-2),
/// and multi-element cases. Bugs in chain walking are length-independent.
const MAX_DESC_LENGTH: usize = 4;

#[kani::proof]
#[kani::unwind(5)]  // MAX_DESC_LENGTH + 1
fn proof_iovec_read() {
    let nr_descs: usize = kani::any_where(|&n| n <= MAX_DESC_LENGTH);
    // ...
}
```

### Specific Error Condition Assertions

In negative proofs, don't just check `Err(_)` — assert which condition triggered the error:

```rust
if queue.add_used(index, kani::any()).is_ok() {
    assert_eq!(queue.next_used, old_next + Wrapping(1));
} else {
    // State unchanged on error
    assert_eq!(queue.next_used, old_next);
    // Error was specifically due to bounds violation
    assert!(index >= queue.size);
}
```

### Bare `kani::cover!()` as Liveness Check

Use `kani::cover!()` without a condition on error paths to verify they are reachable. If assumptions over-constrain, this fails UNSATISFIABLE, revealing the bug:

```rust
if result == BucketReduction::Failure {
    kani::cover!();  // Fails if no execution can reach this path
    assert!(bucket.budget < cost);
}
```

## Kani Limitations to Work Around

### Cannot model OS syscalls

Functions that call `mmap`, `sysconf`, `ioctl`, etc. are not modelable. Extract the pure logic into a separate function and verify that:

```rust
// Production: calls GuestMemoryMmap (OS-backed)
fn collect_dirty_pages(mem: &GuestMemoryMmap, ...) { ... }

// Extracted pure logic for Kani:
fn validated_host_slice(ptr: *const u8, len: usize, addr: u64, region_start: u64, region_len: u64) -> Result<&[u8], &str> { ... }

// Proof verifies the extracted function:
#[kani::proof]
fn proof_validated_host_slice_bounds() {
    // Symbolic region + address → verify bounds check
}
```

When OS-dependent types can't be constructed at all in Kani (e.g. `MmapRegion` needs `sysconf`), transmute from a layout-compatible struct as a last resort. Document the assumption and reference the upstream issue:

```rust
// MmapRegionBuilder::build() calls libc::sysconf — cannot stub.
// Transmute is sound here because Kani does not reorder repr(Rust) fields.
// TODO: replace when kani supports foreign function stubs (#XYZ)
let stub = MmapRegionStub { addr, size, ... };
let region: MmapRegion<()> = unsafe { std::mem::transmute(stub) };
```

### Vec/collection bounds

Kani models heap allocation but with performance limits. Constrain collection sizes:

```rust
let num_pages: usize = kani::any_where(|&n| n > 0 && n <= 256);  // Keep bounded
```

### Custom Arbitrary for complex types

When a type has invariants that `derive(Arbitrary)` cannot express:

```rust
#[cfg(kani)]
impl kani::Arbitrary for UffdRegion {
    fn any() -> Self {
        let guest_addr: u64 = kani::any();
        let host_addr: u64 = kani::any();
        let size: u64 = kani::any_where(|&s| s > 0);
        kani::assume(guest_addr.checked_add(size).is_some());
        kani::assume(host_addr.checked_add(size).is_some());
        UffdRegion { guest_addr, host_addr, size, page_offset: 0 }
    }
}
```

## Proof Design Checklist

Work through this for every proof:

- [ ] **Names the property**: doc comment states exactly what invariant is verified
- [ ] **Calls production code**: exercises actual functions, not reimplementations
- [ ] **Would fail if code changed**: doc comment states the specific production change that breaks it (e.g., "change `fetch_or` to `fetch_add`")
- [ ] **Not tautological**: "wrong implementation" mental test passes
- [ ] **Has meaningful coverage**: `kani::cover!()` on specific conditions, not just `true`
- [ ] **Documents unwind bound**: explains why N is sufficient
- [ ] **Boundary cases**: word boundaries, zero, max values, off-by-one
- [ ] **Negative cases**: invalid inputs produce correct errors
- [ ] **Uses strongest coupling**: contracts > round-trips > direct calls > reimplementation

## Common Proof Categories

### Bitmap/Bitfield Operations
- Mark → is_set round-trip
- Mark isolation (page A doesn't affect page B)
- Word boundary: last bit of word N, first bit of word N+1
- Out-of-bounds silently ignored
- Idempotency (OR-based mark is idempotent)
- Drain/reset lifecycle
- Count equals popcount

### Serialization Round-Trips
- Write → read identity for each width (u16, u32, u64, i32)
- Endianness (LE and BE)
- Contract: buffer length precondition

### Header/Format Validation
- Invalid magic → specific error
- Invalid version → specific error
- Valid header → Ok
- Field independence (unrelated fields don't affect validation)
- All error paths reachable (coverage)
- Short-circuit order (magic checked before version)

### Address Translation
- Guest → host → guest round-trip (invertibility)
- Before-region returns None
- After-region returns None
- Page index arithmetic correct
- Multi-region non-overlapping

### Bounds-Checked Accessors
- Returns Some iff buffer has sufficient length
- Each size threshold verified independently
- Security-critical: verify fix for specific vulnerability classes
