// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::ops::ControlFlow;

use oxc_allocator::Allocator;
use oxc_ast::ast::RegExpFlags;
use oxc_regular_expression::{LiteralParser, Options};
use wtf8::{CodePoint, Wtf8Buf};

use crate::{
    ecmascript::{
        Agent, ArgumentsList, Array, BUILTIN_STRING_MEMORY, ExceptionType, Function, InternalSlots,
        JsResult, Number, Object, PropertyKey, PropertyLookupCache, ProtoIntrinsics, RegExp,
        RegExpHeapData, RegExpLastIndex, String, TryError, TryGetResult, Value, array_create,
        call_function, handle_try_get_result, is_callable, ordinary_create_from_constructor,
        ordinary_object_create_null, throw_set_error, to_length, to_string,
        try_create_data_property_or_throw, try_get, try_result_into_js, try_to_length, unwrap_try,
        unwrap_try_get_value,
    },
    engine::{Bindable, GcScope, NoGcScope, Scopable, Scoped, bindable_handle},
    heap::{ArenaAccess, ArenaAccessMut, CreateHeapData, DirectArenaAccessMut},
};

/// ### [22.2.3.1 RegExpCreate ( P, F )](https://tc39.es/ecma262/#sec-regexpcreate)
///
/// The abstract operation RegExpCreate takes arguments P (an ECMAScript
/// language value) and F (a String or undefined) and returns either a normal
/// completion containing an Object or a throw completion.
pub(crate) fn reg_exp_create<'a>(
    agent: &mut Agent,
    p: Scoped<Value>,
    f: Option<String>,
    gc: GcScope<'a, '_>,
) -> JsResult<'a, RegExp<'a>> {
    let f = f.map_or(Ok(RegExpFlags::empty()), |f| {
        Err(Value::from(f).scope(agent, gc.nogc()))
    });
    // 1. Let obj be ! RegExpAlloc(%RegExp%).
    let obj: RegExp = agent.heap.create(RegExpHeapData::default()).bind(gc.nogc());
    obj.create_backing_object(agent);
    // 2. Return ? RegExpInitialize(obj, P, F).
    reg_exp_initialize(agent, obj.unbind(), p, f, gc)
}

/// ### [22.2.3.1 RegExpCreate ( P, F )](https://tc39.es/ecma262/#sec-regexpcreate)
///
/// The abstract operation RegExpCreate takes arguments P (an ECMAScript
/// language value) and F (a String or undefined) and returns either a normal
/// completion containing an Object or a throw completion.
///
/// This is a variant for RegExp literal creation that cannot fail and skips
/// all of the abstract operation busy-work.
pub(crate) fn reg_exp_create_literal<'a>(
    agent: &mut Agent,
    p: String,
    f: Option<RegExpFlags>,
    gc: NoGcScope<'a, '_>,
) -> RegExp<'a> {
    // 1. Let obj be ! RegExpAlloc(%RegExp%).
    // 2. Return ? RegExpInitialize(obj, P, F).
    let f = f.unwrap_or(RegExpFlags::empty());
    let obj: RegExp = agent.heap.create(RegExpHeapData::new(agent, p, f));
    obj.create_backing_object(agent);
    obj.bind(gc)
}

/// ### [22.2.3.2 RegExpAlloc ( newTarget )](https://tc39.es/ecma262/#sec-regexpalloc)
///
/// The abstract operation RegExpAlloc takes argument newTarget (a constructor)
/// and returns either a normal completion containing an Object or a throw
/// completion.
pub(crate) fn reg_exp_alloc<'a>(
    agent: &mut Agent,
    new_target: Function,
    gc: GcScope<'a, '_>,
) -> JsResult<'a, RegExp<'a>> {
    // 1. Let obj be ? OrdinaryCreateFromConstructor(newTarget, "%RegExp.prototype%", « [[OriginalSource]], [[OriginalFlags]], [[RegExpRecord]], [[RegExpMatcher]] »).
    let obj = RegExp::try_from(ordinary_create_from_constructor(
        agent,
        new_target,
        ProtoIntrinsics::RegExp,
        gc,
    )?)
    .unwrap();
    // 2. Perform ! DefinePropertyOrThrow(obj, "lastIndex", PropertyDescriptor { [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }).
    // 3. Return obj.
    Ok(obj)
}

/// ### [22.2.3.3 RegExpInitialize ( obj, pattern, flags )](https://tc39.es/ecma262/#sec-regexpinitialize)
///
/// The abstract operation RegExpInitialize takes arguments obj (an Object),
/// pattern (an ECMAScript language value), and flags (an ECMAScript language
/// value) and returns either a normal completion containing an Object or a
/// throw completion.
pub(crate) fn reg_exp_initialize<'a>(
    agent: &mut Agent,
    obj: RegExp,
    scoped_pattern: Scoped<Value>,
    scoped_flags: Result<RegExpFlags, Scoped<Value>>,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, RegExp<'a>> {
    let obj = obj.bind(gc.nogc());
    // SAFETY: not shared.
    let pattern = unsafe { scoped_pattern.take(agent).bind(gc.nogc()) };
    let flags = scoped_flags
        .as_ref()
        .map(|f| *f)
        .map_err(|v| v.get(agent).bind(gc.nogc()));
    let quick_pattern = if pattern.is_undefined() {
        Some(None)
    } else if let Ok(pattern) = String::try_from(pattern) {
        Some(Some(pattern))
    } else {
        None
    };
    let quick_flags = match flags {
        Ok(f) => Some(Ok(f)),
        Err(flags) => {
            if flags.is_undefined() {
                Some(Ok(RegExpFlags::empty()))
            } else if let Ok(f) = String::try_from(flags) {
                Some(Err(f))
            } else {
                None
            }
        }
    };
    let (obj, p, f) = if let (Some(p), Some(f)) = (quick_pattern, quick_flags) {
        let p = p.unwrap_or(String::EMPTY_STRING);
        (obj, p, f)
    } else {
        let obj = obj.scope(agent, gc.nogc());
        let flags = scoped_flags;
        // 1. If pattern is undefined, let P be the empty String.
        let mut p = if pattern.is_undefined() {
            String::EMPTY_STRING
        } else {
            // 2. Else, let P be ? ToString(pattern).
            to_string(agent, pattern.unbind(), gc.reborrow())
                .unbind()?
                .bind(gc.nogc())
        };
        let f = match flags {
            Ok(f) => Ok(f),
            Err(flags) => {
                let flags = unsafe { flags.take(agent) }.bind(gc.nogc());
                // 3. If flags is undefined,
                if flags.is_undefined() {
                    // let F be the empty String.
                    Ok(RegExpFlags::empty())
                } else {
                    // 4. Else, let F be ? ToString(flags).
                    let scoped_p = p.scope(agent, gc.nogc());
                    let f = to_string(agent, flags.unbind(), gc.reborrow())
                        .unbind()?
                        .bind(gc.nogc());
                    p = unsafe { scoped_p.take(agent) }.bind(gc.nogc());
                    Err(f)
                }
            }
        };
        let obj = unsafe { obj.take(agent) }.bind(gc.nogc());
        (obj, p, f)
    };
    // 5. If F contains any code unit other than "d", "g", "i", "m", "s", "u",
    //    "v", or "y", or if F contains any code unit more than once, throw a
    //    SyntaxError exception.
    // 6. If F contains "i", let i be true; else let i be false.
    // 7. If F contains "m", let m be true; else let m be false.
    // 8. If F contains "s", let s be true; else let s be false.
    // 9. If F contains "u", let u be true; else let u be false.
    // 10. If F contains "v", let v be true; else let v be false.
    // 11. If u is true or v is true, then
    //     a. Let patternText be StringToCodePoints(P).
    // 12. Else,
    //     a. Let patternText be the result of interpreting each of P's 16-bit
    //     elements as a Unicode BMP code point. UTF-16 decoding is not applied
    //     to the elements.
    let f_str = f.map(|f| f.to_inline_string());
    let f_str = match &f_str {
        Ok(f) => f.as_str().into(),
        Err(f) => f.to_string_lossy_(agent),
    };
    let flags: Option<&str> = if f_str.is_empty() {
        None
    } else {
        Some(f_str.as_ref())
    };

    let allocator = Allocator::new();
    // 13. Let parseResult be ParsePattern(patternText, u, v).
    match LiteralParser::new(
        &allocator,
        &p.to_string_lossy_(agent),
        flags,
        Options::default(),
    )
    .parse()
    {
        Ok(_) => {
            // 15. Assert: parseResult is a Pattern Parse Node.
        }
        // 14. If parseResult is a non-empty List of SyntaxError objects,
        Err(err) => {
            // throw a SyntaxError exception.
            return Err(agent.throw_exception(
                ExceptionType::SyntaxError,
                err.message.to_string(),
                gc.into_nogc(),
            ));
        }
    };
    let f = f.unwrap_or_else(|f| parse_flags(&f.to_string_lossy_(agent)).unwrap());
    // 18. Let capturingGroupsCount be CountLeftCapturingParensWithin(parseResult).
    // 19. Let rer be the RegExp Record { [[IgnoreCase]]: i, [[Multiline]]: m, [[DotAll]]: s, [[Unicode]]: u, [[UnicodeSets]]: v, [[CapturingGroupsCount]]: capturingGroupsCount }.
    // 21. Set obj.[[RegExpMatcher]] to CompilePattern of parseResult with argument rer.
    let reg_exp_matcher = RegExpHeapData::compile_pattern(&p.to_string_lossy_(agent), f);
    {
        let data = obj.get_mut(agent);
        // 16. Set obj.[[OriginalSource]] to P.
        data.original_source = p.unbind();
        // 17. Set obj.[[OriginalFlags]] to F.
        data.original_flags = f;
        // 20. Set obj.[[RegExpRecord]] to rer.
        // 21. Set obj.[[RegExpMatcher]] to CompilePattern of parseResult with argument rer.
        data.reg_exp_matcher = reg_exp_matcher;
    }

    // 22. Perform ? Set(obj, "lastIndex", +0𝔽, true).
    if !obj.set_last_index(agent, RegExpLastIndex::ZERO, gc.nogc()) {
        return throw_set_error(
            agent,
            BUILTIN_STRING_MEMORY.lastIndex.to_property_key(),
            gc.into_nogc(),
        )
        .into();
    }
    // 23. Return obj.
    Ok(obj.unbind())
}

fn parse_flags(f: &str) -> Option<RegExpFlags> {
    let mut flags: u8 = 0;
    for cu in f.as_bytes() {
        match cu {
            b'd' => flags |= RegExpFlags::D.bits(),
            b'g' => flags |= RegExpFlags::G.bits(),
            // 6. If F contains "i", let i be true; else let i be false.
            b'i' => flags |= RegExpFlags::I.bits(),
            // 7. If F contains "m", let m be true; else let m be false.
            b'm' => flags |= RegExpFlags::M.bits(),
            // 8. If F contains "s", let s be true; else let s be false.
            b's' => flags |= RegExpFlags::S.bits(),
            // 9. If F contains "u", let u be true; else let u be false.
            b'u' => flags |= RegExpFlags::U.bits(),
            // 10. If F contains "v", let v be true; else let v be false.
            b'v' => flags |= RegExpFlags::V.bits(),
            b'y' => flags |= RegExpFlags::Y.bits(),
            // 5. If F contains any code unit other than "d", "g", "i", "m",
            //    "s", "u", "v", or "y", or if F contains any code unit more
            //    than once, throw a SyntaxError exception.
            _ => return None,
        }
    }
    Some(RegExpFlags::from_bits_retain(flags))
}

#[inline]
pub(crate) fn require_internal_slot_reg_exp<'a>(
    agent: &mut Agent,
    o: Value,
    gc: NoGcScope<'a, '_>,
) -> JsResult<'a, RegExp<'a>> {
    // 1. If O is not an Object, throw a TypeError exception.
    let Ok(o) = Object::try_from(o) else {
        let error_message = format!(
            "{} is not an object",
            o.unbind()
                .try_string_repr(agent, gc)
                .to_string_lossy_(agent)
        );
        return Err(agent.throw_exception(ExceptionType::TypeError, error_message, gc));
    };
    require_internal_slot_reg_exp_object(agent, o, gc)
}

#[inline]
pub(crate) fn require_internal_slot_reg_exp_object<'a>(
    agent: &mut Agent,
    o: Object,
    gc: NoGcScope<'a, '_>,
) -> JsResult<'a, RegExp<'a>> {
    match o {
        // 1. Perform ? RequireInternalSlot(O, [[RegExpMatcher]]).
        Object::RegExp(reg_exp) => Ok(reg_exp.unbind().bind(gc)),
        _ => Err(agent.throw_exception_with_static_message(
            ExceptionType::TypeError,
            "Expected this to be RegExp",
            gc,
        )),
    }
}

/// ### [22.2.7.1 RegExpExec ( R, S )](https://tc39.es/ecma262/#sec-regexpexec)
///
/// The abstract operation RegExpExec takes arguments R (an Object) and S (a
/// String) and returns either a normal completion containing either an Object
/// or null, or a throw completion.
///
/// > NOTE: If a callable "exec" property is not found this algorithm falls
/// > back to attempting to use the built-in RegExp matching algorithm. This
/// > provides compatible behaviour for code written for prior editions where
/// > most built-in algorithms that use regular expressions did not perform a
/// > dynamic property lookup of "exec".
pub(crate) fn reg_exp_exec<'a>(
    agent: &mut Agent,
    r: Object,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, Option<Object<'a>>> {
    let (r, s) = match reg_exp_exec_prepare(agent, r, s, gc.reborrow()) {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break(result) => return result.unbind().bind(gc.into_nogc()),
    };

    // 4. Return ? RegExpBuiltinExec(R, S).
    reg_exp_builtin_exec(agent, r.unbind(), s.unbind(), gc).map(|o| o.map(|o| o.into()))
}

/// Performs steps 1-3 of RegExpExec
fn reg_exp_exec_prepare<'a>(
    agent: &mut Agent,
    r: Object,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> ControlFlow<JsResult<'a, Option<Object<'a>>>, (RegExp<'a>, String<'a>)> {
    let mut s = s.bind(gc.nogc());
    let mut r = r.bind(gc.nogc());
    // 1. Let exec be ? Get(R, "exec").
    let key = BUILTIN_STRING_MEMORY.exec.to_property_key();
    let exec = try_get(
        agent,
        r,
        key,
        PropertyLookupCache::get(agent, key),
        gc.nogc(),
    );
    let exec = match exec {
        ControlFlow::Continue(TryGetResult::Unset) => Value::Undefined,
        ControlFlow::Continue(TryGetResult::Value(v)) => v,
        ControlFlow::Break(TryError::Err(e)) => {
            return ControlFlow::Break(Err(e.unbind().bind(gc.into_nogc())));
        }
        _ => {
            let scoped_r = r.scope(agent, gc.nogc());
            let scoped_s = s.scope(agent, gc.nogc());
            let exec = handle_try_get_result(
                agent,
                r.unbind(),
                BUILTIN_STRING_MEMORY.exec.to_property_key(),
                exec.unbind(),
                gc.reborrow(),
            )
            .unbind()
            .bind(gc.nogc());
            let exec = match exec {
                Ok(e) => e,
                Err(err) => return ControlFlow::Break(Err(err.unbind())),
            };
            let gc = gc.nogc();
            // SAFETY: Not shared.
            unsafe {
                s = scoped_s.take(agent).bind(gc);
                r = scoped_r.take(agent).bind(gc);
            }
            exec
        }
    };

    // Fast path: native RegExp object and intrinsic exec function.
    if let Object::RegExp(r) = r
        && exec
            == agent
                .current_realm_record()
                .intrinsics()
                .reg_exp_prototype_exec()
                .into()
    {
        return ControlFlow::Continue((r.unbind(), s.unbind()));
    }

    // 2. If IsCallable(exec) is true, then
    if let Some(exec) = is_callable(exec, gc.nogc()) {
        // a. Let result be ? Call(exec, R, « S »).
        let result = call_function(
            agent,
            exec.unbind(),
            r.unbind().into(),
            Some(ArgumentsList::from_mut_value(&mut s.unbind().into())),
            gc.reborrow(),
        )
        .unbind();
        let gc = gc.into_nogc();
        let result = result.bind(gc);
        let result = match result {
            Ok(r) => r,
            Err(err) => return ControlFlow::Break(Err(err)),
        };
        // b. If result is not an Object and result is not null,
        let result = if let Ok(result) = Object::try_from(result) {
            Some(result)
        } else if result.is_null() {
            None
        } else {
            // throw a TypeError exception.
            return ControlFlow::Break(Err(agent.throw_exception_with_static_message(
                ExceptionType::TypeError,
                "'exec' function result was not object or null",
                gc,
            )));
        };
        // c. Return result.
        return ControlFlow::Break(Ok(result));
    }
    // 3. Perform ? RequireInternalSlot(R, [[RegExpMatcher]]).
    let r = require_internal_slot_reg_exp_object(agent, r, gc.nogc()).unbind();
    let s = s.unbind();
    let gc = gc.into_nogc();
    let r = r.bind(gc);
    let s = s.bind(gc);
    let r = match r {
        Ok(r) => r,
        Err(err) => return ControlFlow::Break(Err(err)),
    };
    ControlFlow::Continue((r, s))
}

pub(crate) fn reg_exp_test<'a>(
    agent: &mut Agent,
    r: Object,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, bool> {
    let (r, s) = match reg_exp_exec_prepare(agent, r, s, gc.reborrow()) {
        ControlFlow::Continue(r) => r,
        ControlFlow::Break(result) => {
            return result.unbind().bind(gc.into_nogc()).map(|r| r.is_some());
        }
    };
    // 4. Return ? RegExpBuiltinExec(R, S).
    reg_exp_builtin_test(agent, r.unbind(), s.unbind(), gc)
}

pub(crate) struct RegExpExecBase<'gc> {
    pub(crate) r: RegExp<'gc>,
    pub(crate) s: String<'gc>,
    pub(crate) last_index: usize,
    pub(crate) global: bool,
    pub(crate) sticky: bool,
    pub(crate) has_indices: bool,
    pub(crate) full_unicode: bool,
}

bindable_handle!(RegExpExecBase);

pub(crate) fn reg_exp_builtin_exec_prepare<'a>(
    agent: &mut Agent,
    r: RegExp,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, RegExpExecBase<'a>> {
    let mut r = r.bind(gc.nogc());
    let mut s = s.bind(gc.nogc());

    // 1. Let length be the length of S.
    // 2. Let lastIndex be ℝ(? ToLength(? Get(R, "lastIndex"))).
    let mut last_index = if let Some(last_index) = r.try_get_last_index(agent) {
        last_index as usize
    } else {
        // Note: calling Get(R, "lastIndex") cannot trigger JavaScript
        // execution, as the "lastIndex" property is always an unconfigurable
        // data property of every RegExp object.
        let last_index = unwrap_try_get_value(try_get(
            agent,
            r,
            BUILTIN_STRING_MEMORY.lastIndex.to_property_key(),
            None,
            gc.nogc(),
        ));
        if let Some(last_index) =
            try_result_into_js(try_to_length(agent, last_index, gc.nogc())).unbind()?
        {
            last_index as usize
        } else {
            let scoped_r = r.scope(agent, gc.nogc());
            let scoped_s = s.scope(agent, gc.nogc());
            let last_index =
                to_length(agent, last_index.unbind(), gc.reborrow()).unbind()? as usize;
            // SAFETY: Not shared.
            unsafe {
                s = scoped_s.take(agent).bind(gc.nogc());
                r = scoped_r.take(agent).bind(gc.nogc());
            }
            last_index
        }
    };
    let r = r.unbind();
    let s = s.unbind();
    let gc = gc.into_nogc();
    let r = r.bind(gc);
    let s = s.bind(gc);

    // 3. Let flags be R.[[OriginalFlags]].
    let flags = r.original_flags(agent);
    // 4. If flags contains "g", let global be true; else let global be false.
    let global = (flags & RegExpFlags::G).bits() > 0;
    // 5. If flags contains "y", let sticky be true; else let sticky be false.
    let sticky = (flags & RegExpFlags::Y).bits() > 0;
    // 6. If flags contains "d", let hasIndices be true; else let hasIndices be false.
    let has_indices = (flags & RegExpFlags::D).bits() > 0;
    // 7. If global is false and sticky is false, set lastIndex to 0.
    if !global && !sticky {
        last_index = 0;
    }
    // `last_index` stays a UTF-16 code-unit index: regress's `find_from_*` takes
    // a code-unit start and reports code-unit positions, and exec compares it
    // against the UTF-16 length. (No WTF-8 byte conversion, so no out-of-range
    // map indexing.)
    // 8. Let matcher be R.[[RegExpMatcher]].
    if let Err(err) = &r.get(agent).reg_exp_matcher {
        return Err(agent.throw_exception(ExceptionType::SyntaxError, err.to_string(), gc));
    };
    // 9. If flags contains "u" or flags contains "v", let fullUnicode be true;
    //    else let fullUnicode be false.
    let full_unicode = (flags & (RegExpFlags::U | RegExpFlags::V)).bits() > 0;
    Ok(RegExpExecBase {
        r,
        s,
        last_index,
        global,
        sticky,
        has_indices,
        full_unicode,
    })
}

/// ### [22.2.7.2 RegExpBuiltinExec ( R, S )](https://tc39.es/ecma262/#sec-regexpbuiltinexec)
///
/// The abstract operation RegExpBuiltinExec takes arguments R (an initialized
/// RegExp instance) and S (a String) and returns either a normal completion
/// containing either an Array exotic object or null, or a throw completion.
pub(crate) fn reg_exp_builtin_exec<'a>(
    agent: &mut Agent,
    r: RegExp,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, Option<Array<'a>>> {
    let r = r.bind(gc.nogc());
    let s = s.bind(gc.nogc());
    let result =
        reg_exp_builtin_exec_prepare(agent, r.unbind(), s.unbind(), gc.reborrow()).unbind()?;
    let gc = gc.into_nogc();
    let RegExpExecBase {
        r,
        s,
        last_index,
        global,
        sticky,
        has_indices,
        full_unicode,
    } = result.bind(gc);
    // 1. Let length be the length of S, in UTF-16 code units.
    let length = s.utf16_len_(agent);
    // 13.a. If lastIndex > length, then
    if last_index > length {
        // i. If global is true or sticky is true, set lastIndex to 0.
        if global || sticky {
            r.get_direct_mut(&mut agent.heap.regexps).last_index = RegExpLastIndex::ZERO;
        }
        // ii. Return null.
        return Ok(None);
    }
    // Feed regress the string as UTF-16 code units; it reports matches as
    // code-unit ranges, so `.index`/lastIndex/captures need no WTF-8 byte
    // conversion. (regress is the ECMAScript-spec backtracking engine; unlike
    // the `regex` crate it supports lookahead/lookbehind/backreferences.)
    let units: Vec<u16> = s
        .as_wtf8_(&agent.heap.strings)
        .to_ill_formed_utf16()
        .collect();
    // 8. Let matcher be R.[[RegExpMatcher]]. 13.c. Let r be matcher(input, lastIndex).
    let m: Option<regress::Match> = {
        let r_data = r.get_direct_mut(&mut agent.heap.regexps);
        // SAFETY: reg_exp_builtin_exec_prepare checks the matcher is set.
        let matcher = unsafe { r_data.reg_exp_matcher.as_ref().unwrap_unchecked() };
        // fullUnicode pairs surrogates into code points; otherwise each code
        // unit is a character (ucs2). Both report positions in code units.
        if full_unicode {
            matcher.find_from_utf16(&units, last_index).next()
        } else {
            matcher.find_from_ucs2(&units, last_index).next()
        }
    };
    // 13.d. If r is failure, then reset (if g/y) and return null.
    let Some(m) = m else {
        if global || sticky {
            r.get_direct_mut(&mut agent.heap.regexps).last_index = RegExpLastIndex::ZERO;
        }
        return Ok(None);
    };
    // regress has no native sticky flag, so emulate it: a sticky match must
    // begin exactly at lastIndex.
    if sticky && m.start() != last_index {
        r.get_direct_mut(&mut agent.heap.regexps).last_index = RegExpLastIndex::ZERO;
        return Ok(None);
    }
    // match start/end are UTF-16 code-unit indices (GetStringIndex is implicit).
    let match_start = m.start();
    let e = m.end();
    // 16. If global is true or sticky is true, set lastIndex to e.
    if global || sticky {
        r.get_direct_mut(&mut agent.heap.regexps).last_index = e.into();
    }
    // 17. Let n be the number of capturing groups (excluding the whole match).
    let n = m.captures.len();
    // 19. Assert: n < 2**32 - 1.
    debug_assert!(n < 2usize.pow(32) - 1);
    // Named groups, owned so the borrow on `m` ends before we touch the heap.
    let named_groups: Vec<(std::string::String, Option<core::ops::Range<usize>>)> = m
        .named_groups()
        .map(|(name, range)| (name.to_string(), range))
        .collect();
    let has_group_name = !named_groups.is_empty();
    // 20. Let A be ! ArrayCreate(n + 1) (slot 0 is the whole match).
    let a = array_create(agent, n + 1, n + 1, None, gc).unwrap();
    // 21. A has n + 1 elements: the whole match plus n capture groups.
    debug_assert_eq!(a.len(agent) as usize, n + 1);
    // 22. CreateDataPropertyOrThrow(A, "index", 𝔽(matchStart)) — code units.
    unwrap_try(try_create_data_property_or_throw(
        agent,
        a,
        BUILTIN_STRING_MEMORY.index.to_property_key(),
        Number::try_from(match_start).unwrap().into(),
        None,
        gc,
    ));
    // 23. CreateDataPropertyOrThrow(A, "input", S).
    let input_key = String::from_static_str(agent, "input", gc).to_property_key();
    unwrap_try(try_create_data_property_or_throw(
        agent,
        a,
        input_key,
        s.into(),
        None,
        gc,
    ));
    // 30/31. groups is a null-proto object iff any capture group is named.
    let groups = if has_group_name {
        Some(ordinary_object_create_null(agent, gc))
    } else {
        None
    };
    // 28/29. A[0] is the matched substring (rebuilt from the code-unit range,
    // so a split surrogate survives as a lone surrogate).
    let matched = code_units_substring(agent, &units, match_start..e, gc);
    unwrap_try(try_create_data_property_or_throw(
        agent,
        a,
        PropertyKey::from(0u32),
        matched.into(),
        None,
        gc,
    ));
    // 34. For each 1 ≤ i ≤ n, A[i] is the ith capture (a substring or undefined).
    for i in 1..=n {
        let captured_value = match m.captures[i - 1].clone() {
            Some(range) => code_units_substring(agent, &units, range, gc).into(),
            None => Value::Undefined,
        };
        unwrap_try(try_create_data_property_or_throw(
            agent,
            a,
            PropertyKey::try_from(i).unwrap(),
            captured_value,
            None,
            gc,
        ));
    }
    // 30.e. Populate the groups object from the named captures.
    if let Some(groups) = groups {
        for (name, range) in &named_groups {
            let value = match range.clone() {
                Some(range) => code_units_substring(agent, &units, range, gc).into(),
                None => Value::Undefined,
            };
            let key = String::from_str(agent, name, gc).to_property_key();
            unwrap_try(try_create_data_property_or_throw(
                agent, groups, key, value, None, gc,
            ));
        }
    }
    // 32. CreateDataPropertyOrThrow(A, "groups", groups).
    let groups_key = String::from_static_str(agent, "groups", gc).to_property_key();
    unwrap_try(try_create_data_property_or_throw(
        agent,
        a,
        groups_key,
        groups.map_or(Value::Undefined, |g| g.into()),
        None,
        gc,
    ));
    // 35. If hasIndices (the `d` flag) is set, attach the "indices" array
    // (MakeMatchIndicesIndexPairArray). Each element is a [start, end] code-unit
    // pair (or undefined for an unmatched group); .groups mirrors the named
    // captures. regress gives the ranges directly, so no byte conversion.
    if has_indices {
        let indices = array_create(agent, n + 1, n + 1, None, gc).unwrap();
        let whole = index_pair(agent, match_start, e, gc);
        unwrap_try(try_create_data_property_or_throw(
            agent,
            indices,
            PropertyKey::from(0u32),
            whole.into(),
            None,
            gc,
        ));
        for i in 1..=n {
            let value = match m.captures[i - 1].clone() {
                Some(range) => index_pair(agent, range.start, range.end, gc).into(),
                None => Value::Undefined,
            };
            unwrap_try(try_create_data_property_or_throw(
                agent,
                indices,
                PropertyKey::try_from(i).unwrap(),
                value,
                None,
                gc,
            ));
        }
        let idx_groups = if has_group_name {
            Some(ordinary_object_create_null(agent, gc))
        } else {
            None
        };
        if let Some(idx_groups) = idx_groups {
            for (name, range) in &named_groups {
                let value = match range.clone() {
                    Some(range) => index_pair(agent, range.start, range.end, gc).into(),
                    None => Value::Undefined,
                };
                let key = String::from_str(agent, name, gc).to_property_key();
                unwrap_try(try_create_data_property_or_throw(
                    agent, idx_groups, key, value, None, gc,
                ));
            }
        }
        let idx_groups_key = String::from_static_str(agent, "groups", gc).to_property_key();
        unwrap_try(try_create_data_property_or_throw(
            agent,
            indices,
            idx_groups_key,
            idx_groups.map_or(Value::Undefined, |g| g.into()),
            None,
            gc,
        ));
        let indices_key = String::from_static_str(agent, "indices", gc).to_property_key();
        unwrap_try(try_create_data_property_or_throw(
            agent,
            a,
            indices_key,
            indices.into(),
            None,
            gc,
        ));
    }
    // 36. Return A.
    Ok(Some(a))
}

/// A `[start, end]` two-element index pair (code units) for the `d` flag's
/// `indices` array.
fn index_pair<'gc>(
    agent: &mut Agent,
    start: usize,
    end: usize,
    gc: NoGcScope<'gc, '_>,
) -> Array<'gc> {
    let pair = array_create(agent, 2, 2, None, gc).unwrap();
    unwrap_try(try_create_data_property_or_throw(
        agent,
        pair,
        PropertyKey::from(0u32),
        Number::try_from(start).unwrap().into(),
        None,
        gc,
    ));
    unwrap_try(try_create_data_property_or_throw(
        agent,
        pair,
        PropertyKey::from(1u32),
        Number::try_from(end).unwrap().into(),
        None,
        gc,
    ));
    pair
}

/// Build a JS String from a UTF-16 code-unit range of `units`. Lone surrogates
/// (e.g. from slicing through a pair) are preserved via WTF-8.
fn code_units_substring<'gc>(
    agent: &mut Agent,
    units: &[u16],
    range: core::ops::Range<usize>,
    gc: NoGcScope<'gc, '_>,
) -> String<'gc> {
    let mut buf = Wtf8Buf::new();
    for cp in char::decode_utf16(units[range].iter().copied()) {
        match cp {
            Ok(c) => buf.push_char(c),
            // SAFETY: decode_utf16 only errors on an unpaired surrogate, which
            // is a valid WTF-8 CodePoint.
            Err(e) => {
                buf.push(unsafe { CodePoint::from_u32_unchecked(e.unpaired_surrogate() as u32) })
            }
        }
    }
    String::from_wtf8_buf(agent, buf, gc)
}

pub(crate) fn reg_exp_builtin_test<'a>(
    agent: &mut Agent,
    r: RegExp,
    s: String,
    mut gc: GcScope<'a, '_>,
) -> JsResult<'a, bool> {
    let r = r.bind(gc.nogc());
    let s = s.bind(gc.nogc());
    // if (r.get(agent).original_flags & (RegExpFlags::G | RegExpFlags::S)).bits() > 0 {
    //     // We have to perform the actual matching because global and sticky
    //     // RegExp's can observe its match results from lastIndex.
    //     return reg_exp_builtin_exec(agent, r.unbind(), s.unbind(), gc).map(|a| a.is_some());
    // }
    let result =
        reg_exp_builtin_exec_prepare(agent, r.unbind(), s.unbind(), gc.reborrow()).unbind()?;
    let gc = gc.into_nogc();
    let RegExpExecBase {
        r,
        s,
        last_index,
        sticky,
        global,
        full_unicode,
        ..
    } = result.bind(gc);

    // 1. Let length be the length of S, in UTF-16 code units.
    let length = s.utf16_len_(agent);
    if last_index > length {
        if global || sticky {
            r.get_direct_mut(&mut agent.heap.regexps).last_index = RegExpLastIndex::ZERO;
        }
        return Ok(false);
    }
    // Run regress over the string's UTF-16 code units (see reg_exp_builtin_exec).
    let units: Vec<u16> = s
        .as_wtf8_(&agent.heap.strings)
        .to_ill_formed_utf16()
        .collect();
    let m: Option<regress::Match> = {
        let r_data = r.get_direct_mut(&mut agent.heap.regexps);
        // SAFETY: reg_exp_builtin_exec_prepare checks the matcher is set.
        let matcher = unsafe { r_data.reg_exp_matcher.as_ref().unwrap_unchecked() };
        if full_unicode {
            matcher.find_from_utf16(&units, last_index).next()
        } else {
            matcher.find_from_ucs2(&units, last_index).next()
        }
    };
    let r_data = r.get_direct_mut(&mut agent.heap.regexps);
    // Global and sticky observe the match position via lastIndex; non-global
    // non-sticky always resets lastIndex to 0. (regress has no native sticky,
    // so a sticky match off the start position is treated as no match.)
    match m {
        Some(m) if !(sticky && m.start() != last_index) => {
            r_data.last_index = if global || sticky {
                m.end().into()
            } else {
                RegExpLastIndex::ZERO
            };
            Ok(true)
        }
        _ => {
            r_data.last_index = RegExpLastIndex::ZERO;
            Ok(false)
        }
    }
}

/// ### [22.2.7.3 AdvanceStringIndex ( S, index, unicode )](https://tc39.es/ecma262/#sec-advancestringindex)
///
/// The abstract operation AdvanceStringIndex takes arguments S (a String),
/// index (a non-negative integer), and unicode (a Boolean) and returns an
/// integer.
pub(crate) fn advance_string_index(agent: &Agent, s: String, index: usize, unicode: bool) -> usize {
    // 1. Assert: index ≤ 2**53 - 1.
    assert!(index < 2usize.pow(53));
    // 2. If unicode is false, return index + 1.
    if !unicode {
        return index + 1;
    }
    // 3. Let length be the length of S.
    let length = s.utf16_len_(agent);
    // 4. If index + 1 ≥ length, return index + 1.
    if index + 1 >= length {
        return index + 1;
    }
    // 5. Let cp be CodePointAt(S, index).
    let cp = s.code_point_at_(agent, index);
    // 6. Return index + cp.[[CodeUnitCount]].
    let code = cp.to_u32();
    if (code & !0xFFFF) > 0 {
        // Two-code-unit character.
        index + 2
    } else {
        index + 1
    }
}
