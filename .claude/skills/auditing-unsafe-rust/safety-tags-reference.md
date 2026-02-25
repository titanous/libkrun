# Safety Tags Reference

Full catalog of the safety tag DSL for annotating unsafe Rust APIs.

Each tag has the form `[!]Tag(arg1, arg2, ...)`. The `!` prefix negates (e.g., `!Null(p)` = p must not be null).

## Tag Usage Categories

- **Precondition**: Must hold before calling the API.
- **Hazard**: Dangerous state that exists after calling the API. Temporary violation may be acceptable.
- **Option**: Sufficient but not necessary for safety.

## Primitive Tags

### Layout

| Tag | Meaning | Usage |
|-----|---------|-------|
| `Align(p, T)` | `p` aligned to `T`, `sizeof(T)` multiple of alignment | precondition |
| `Size(T, s)` | `sizeof(T) = s` (number, range, `unknown`, `any`, `!0`) | precondition/option |
| `!Padding(T)` | Type `T` has zero padding bytes | precondition |

### Pointer

| Tag | Meaning | Usage |
|-----|---------|-------|
| `!Null(p)` | `p != 0` | precondition |
| `Allocated(p, T, len, A)` | `[p, p+sizeof(T)*len)` allocated by allocator `A` | precondition |
| `InBound(p, T, len)` | `[p, p+sizeof(T)*len)` within single allocated object | precondition |
| `!Overlap(dst, src, len, T)` | `\|dst-src\| > sizeof(T) * len` | precondition |

### Content

| Tag | Meaning | Usage |
|-----|---------|-------|
| `ValidNum(expr, vrange)` | `expr` in `vrange`, no overflow | precondition |
| `ValidString(arange)` | Memory in `arange` is valid UTF-8 | precondition/hazard |
| `ValidCStr(p, len)` | Byte at `p+len` is `\0` | precondition |
| `Init(p, T, len)` | `len` values of `T` at `p` are initialized | precondition/hazard |
| `Unwrap(x, T)` | `x` safely unwraps to variant `T` | precondition |
| `Typed(p, T)` | Memory at `p` holds valid `T` | precondition |

### Alias

| Tag | Meaning | Usage |
|-----|---------|-------|
| `Owning(p)` | `*p` has no current owner | precondition |
| `Alias(p1, p2)` | `p1` and `p2` alias same memory | hazard |
| `Alive(p, l)` | Lifetime of `*p` >= `l` | precondition |

### Miscellaneous

| Tag | Meaning | Usage |
|-----|---------|-------|
| `Pinned(p, l)` | `*p` not moved for lifetime `l` | hazard |
| `!Volatile(p, T, len)` | No concurrent writes to `[p, p+sizeof(T)*len)` | precondition |
| `Opened(fd)` | File descriptor `fd` is open | precondition |
| `Trait(T, trait)` | `T` implements `trait` | option |
| `!Reachable()` | This code point must be unreachable | precondition |

## Compound Tags

| Tag | Expands To |
|-----|-----------|
| `Deref(p, T, len)` | `Allocated(p, T, len, any) && InBound(p, T, len)` |
| `ValidPtr(p, T, len)` | `Size(T, 0) \|\| (Size(T, !0) && Deref(p, T, len))` |
| `Ptr2Ref(p, T)` | `Align(p, T) && Deref(p, T, 1) && Alias(p, 0)` |
| `Layout(p, layout)` | `ValidNum(rem(p, layout.align), 0) && Allocated(p, u8, layout.size, heap)` |

## Implication Relationships

Use these to eliminate redundant tags:

- `Allocated(p, T, len, any)` implies `!Null(p)`
- `Init(p, T, len)` implies `Allocated(p, T, len, any)`
- `Deref(p, T, len)` implies `Allocated(p, T, len, any)` and `InBound(p, T, len)`
- `ValidPtr(p, T, len)` implies `Deref(p, T, len)` for non-ZST types
- `Ptr2Ref(p, T)` implies `Align(p, T)` and `Deref(p, T, 1)`

## Annotation Examples

```
// slice::from_raw_parts(data: *const T, len: usize) -> &'a [T]
// Tags: Align(data, T), Init(data, T, len), Alias(data, 0),
//       ValidNum(add(data, mul(sizeof(T), len)), (0, isize::MAX])

// Box::from_raw(raw: *mut T) -> Box<T>
// Tags: Align(raw, T), Allocated(raw, T, 1, Global), InBound(raw, T, 1),
//       Alias(raw, 0), Owning(raw)

// str::from_utf8_unchecked(v: &[u8]) -> &str
// Tags: ValidString(v)

// NonNull::new_unchecked(ptr: *mut T) -> NonNull<T>
// Tags: !Null(ptr)
```

## Unsupported Safety Properties

Two categories are not well-captured by this DSL:

- **Unsafe trait implementations** (`Send`, `Sync`, `GlobalAlloc`): Requirements are highly type-specific.
- **Non-language-level UB**: System environment constraints (`env::set_var`), function correctness (`CursorMut::insert_after_unchecked`), compiler intrinsics.

These require case-by-case analysis outside the tag framework.
