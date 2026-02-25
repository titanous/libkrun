---
name: auditing-unsafe-rust
description: Use when reviewing unsafe Rust code, writing # Safety documentation, checking soundness of unsafe API encapsulation, or auditing a crate's unsafe usage - applies structured safety tag annotation and audit unit analysis to find missing, incorrect, or inconsistent safety documentation
user-invocable: true
---

# Auditing Unsafe Rust

Structured methodology for auditing unsafe Rust code, based on the safety tag DSL and audit unit framework from Rao et al. (arXiv:2504.21312). Replaces ad-hoc prose review with systematic annotation, propagation tracking, and rule-based verification.

## When to Use

- Reviewing code that contains `unsafe` blocks or `unsafe fn`
- Writing or reviewing `# Safety` documentation on unsafe APIs
- Checking whether a safe function with interior unsafe code is sound
- Auditing a struct with multiple constructors and methods that use unsafe internally
- Verifying that safety requirements propagate correctly through call chains

## Audit Workflow

Work through these phases in order. Each phase builds on the previous.

### Phase 1: Enumerate and Annotate

1. **Find all public unsafe APIs** in the target scope (function, module, or crate).
2. **Flag any with empty `# Safety` sections.** Every public unsafe API must document its safety requirements.
3. **Translate each prose safety requirement into safety tags** using the DSL. See `safety-tags-reference.md` in this directory for the full tag catalog.
4. **Eliminate redundant tags** using implication rules (e.g., `Allocated(p, T, len, any)` implies `!Null(p)`).
5. **Cross-check with signature-based inference** (below) to catch missing tags.

### Phase 2: Build the Unsafety Propagation Graph (UPG)

The UPG is a directed graph tracking how unsafe code propagates through a crate. For each function with interior unsafe code:

1. **Identify callees**: What unsafe APIs does it call? What are their `RS` sets?
2. **Classify the caller**: Is it safe (encapsulation) or unsafe (delegation)?
3. **For methods**: Find all constructors of the struct. Add constructor-to-method edges.
4. **Identify the kill set**: Find all `&mut self` methods on the struct — these form `KS_M`, the set of safety guarantees that could be invalidated between construction and the target method call.

**Edge cases in UPG construction:**
- **Trait method callees**: When a callee is a method on a generic-typed object meeting a trait bound, consider all trait implementations as potential callees.
- **Function pointer / closure callees**: When a callee is a function parameter (`fn(T) -> U`), consider all functions matching that signature.
- **Multiple constructors**: Each constructor gets its own edge to the method. All must be analyzed.

### Phase 3: Identify Structural Patterns and Form Audit Units

Each call chain in the UPG maps to a **structural pattern** that determines which audit rule applies.

#### Structural Patterns

**One-node (origin):** A leaf unsafe API (`f_u` or `m_u`) with no unsafe callees of its own. This is root unsafe code. It must have documented safety requirements (`RS`).

**Two-node (caller -> callee):** A direct caller of an unsafe function.
- If the caller is **safe** (`f'_s -> f_u`): this is **encapsulation**. The caller must internally satisfy all callee requirements.
- If the caller is **unsafe** (`f'_u -> f_u`): this is **delegation**. The caller must either satisfy or re-document the callee's requirements.

**Three+-node (constructor + method + callee):** When the caller is a dynamic method, the constructor that created the instance must also be considered. Four combinations exist based on whether the constructor and method are safe/unsafe. The constructor's guarantees (`VS_c` or `RS_c U VS_c`) combine with the method's guarantees, minus any guarantees killed by intervening `&mut self` methods (`KS_M`).

#### Merging into Audit Units

Basic patterns merge into audit units to avoid redundant analysis:

1. **Same caller, multiple unsafe callees**: Merge — the caller handles safety for all callees in one unit. The formula unions all callee `RS` sets.
2. **Same method, multiple constructors**: Merge — all constructors must satisfy the method's remaining requirements. The formula intersects `(RS_c U VS_c)` across all constructors.

Use the **simpler 2-node formulas** (Encapsulation/Delegation) when the caller is a static function or a method on a struct with a single constructor. Use the **struct-level formulas** (with constructor intersection) only when multiple constructors exist.

### Phase 4: Apply Audit Rules

For each audit unit, apply the matching rule:

| Situation | Rule | Formula |
|-----------|------|---------|
| Public unsafe API exists | **Annotation** | `RS != {}` (safety docs exist) |
| Safe fn calls unsafe callees | **Encapsulation** | `(U RS_callee) <= VS_caller` |
| Unsafe fn calls unsafe callees | **Delegation** | `(U RS_callee) <= RS_caller U VS_caller` |
| Struct with safe method calling unsafe, multiple constructors C | **Struct Encapsulation** | `(U RS_callee) <= (N_{c in C} (RS_c U VS_c)) U VS_method` |
| Struct with unsafe method calling unsafe, multiple constructors C | **Struct Delegation** | `(U RS_callee) <= (N_{c in C} (RS_c U VS_c)) U RS_method U VS_method` |
| Struct with multiple constructors | **Constructor Consistency** | All constructors provide equivalent `RS U VS` |

Key: `U` = union, `N` = intersection, `<=` = subset-of.

**Encapsulation** means a safe caller fully handles all callee safety requirements internally.
**Delegation** means an unsafe caller passes unhandled requirements to its own callers via `# Safety` docs.

### Phase 5: Flag and Review

Compare documented tags against inferred tags. Flag discrepancies:

- **Missing tags**: Safety requirement exists but isn't documented. Add documentation.
- **False tags**: Documented requirement is incorrect or overly restrictive. Correct it.
- **Soundness issues**: No safe annotation strategy can cover the requirements. Fix the code (e.g., make fields private to disable literal constructors).

## Signature-Based Inference (Quick Reference)

Automatically infer expected safety tags from function signatures and names:

| Pattern | Inferred Tags |
|---------|---------------|
| `unsafe fn(raw_ptr) -> object` / name has `from_raw` | Align, Allocated, InBound, Alias, Owning |
| `unsafe fn(raw_ptr) -> &ref` / name has `as_ref`/`as_mut` | Align, Allocated, InBound, Alias |
| `unsafe fn(raw_ptr, Allocator)` / name has `_in` | Allocator |
| Name has `unchecked`, integer context | ValidNum |
| Name has `unchecked`, str context | ValidString |
| Name has `unchecked`, slice context | InBound |
| Name has `assume_init` | Init |

### Elimination Rules (Reduce False Positives)

| Condition | Action |
|-----------|--------|
| Caller of `unchecked` fn returns `Option<T>` | Remove `unchecked` tags from caller |
| Caller accepts `NonNull<T>`, callee needs `!Null` | Remove `!Null` from caller |
| Enum has literal constructor + `unchecked` unsafe constructor | Literal constructor verifies `unchecked` tags |
| Struct has `from_raw` + non-raw-ptr constructor | Non-raw-ptr constructor doesn't inherit raw-ptr tags |

## Common Bug Patterns

These patterns were found auditing the Rust standard library:

| Pattern | What to Look For |
|---------|-----------------|
| Missing `# Safety` entirely | Public unsafe API with no safety docs |
| Missing `Allocator` tag | `from_raw` that delegates to allocator-aware constructor but doesn't document allocator consistency |
| Missing `Alias`/`Owning` | Raw pointer converted to owned object without alias/ownership warning |
| Missing `ValidString` | `unchecked` UTF-8 conversion without documenting validity requirement |
| False `!Null` for ZSTs | Requiring non-null pointer when zero-sized types make null valid |
| Unsound literal constructor | Struct with public fields allows bypassing unsafe constructor's safety requirements |

## Safety Property Sets (Glossary)

| Symbol | Name | Meaning |
|--------|------|---------|
| `RS_f` | Required Safety | Tags the caller must guarantee before calling `f` |
| `VS_f` | Verified Safety | Tags that `f` enforces internally |
| `KS_M` | Killed Safety | Tags invalidated by `&mut self` methods called between constructor and target method |

## Key Definitions

- **Interior unsafe**: A safe function containing `unsafe` blocks internally
- **Root unsafe code**: Unsafe code not forced by internal callees — the origin of unsafety
- **Audit unit**: Minimal self-contained subgraph with one soundness formula — a single unsafe call chain to verify
- **Literal constructor**: `Type { field: value }` syntax — if fields are public, this bypasses any unsafe constructor's safety requirements
