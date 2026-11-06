// The value basics the Chromium port needs first: the four singletons, the
// `Primitive`/`Numeric` hierarchy node they are typed as, and `Maybe`'s
// `Just`/`Nothing`. Blink's converters reach for `v8::Undefined(isolate)` more
// than any other symbol the facade lacked (33 references in
// `bindings/core/v8`), so this target checks they are the engine's singletons
// rather than lookalikes: a script compares each one against its own literal.
//
// The compile-time half is the hierarchy — `Local<Primitive> p = True(isolate)`
// only compiles when `Boolean` derives from `Primitive`, which is V8 15's shape
// (`v8-primitive.h`).

#include <stdio.h>

#include <memory>

#ifdef _WIN32
#include <crtdbg.h>

namespace {
struct SuppressDialogBoxes {
  SuppressDialogBoxes() {
    _set_abort_behavior(0, _WRITE_ABORT_MSG | _CALL_REPORTFAULT);
    _set_error_mode(_OUT_TO_STDERR);
    _CrtSetReportMode(_CRT_ASSERT, _CRTDBG_MODE_FILE);
    _CrtSetReportFile(_CRT_ASSERT, _CRTDBG_FILE_STDERR);
    _CrtSetReportMode(_CRT_ERROR, _CRTDBG_MODE_FILE);
    _CrtSetReportFile(_CRT_ERROR, _CRTDBG_FILE_STDERR);
  }
};
const SuppressDialogBoxes kSuppressDialogBoxes;
}  // namespace
#endif

#include "include/v8-context.h"
#include "include/v8-function.h"
#include "include/v8-initialization.h"
#include "include/v8-isolate.h"
#include "include/v8-local-handle.h"
#include "include/v8-object.h"
#include "include/v8-primitive.h"
#include "include/v8-script.h"
#include "include/v8-value.h"

namespace {

int g_failures = 0;

void Check(bool ok, const char* what) {
  if (!ok) {
    fprintf(stderr, "FAILED: %s\n", what);
    ++g_failures;
  }
}

// Runs `source` in `context` and answers its result as an int32, or -1.
int32_t RunInt(v8::Isolate* isolate, v8::Local<v8::Context> context,
               const char* source) {
  v8::Local<v8::String> text = v8::String::NewFromUtf8Literal(isolate, source);
  v8::Local<v8::Script> script =
      v8::Script::Compile(context, text).ToLocalChecked();
  v8::Local<v8::Value> result = script->Run(context).ToLocalChecked();
  return result->Int32Value(context).FromMaybe(-1);
}

}  // namespace

int main() {
  // The enumerators carry V8 15's own values, so a host's serialized or
  // documented bit patterns mean the same thing here.
  static_assert(v8::ScriptCompiler::kConsumeCodeCache == 1);
  static_assert(v8::ScriptCompiler::kEagerCompile == 2);
  static_assert(v8::ScriptCompiler::kProduceCompileHints == 4);
  static_assert(v8::ScriptCompiler::kConsumeCompileHints == 8);
  static_assert(v8::ScriptCompiler::kFollowCompileHintsMagicComment == 16);
  static_assert(
      v8::ScriptCompiler::kFollowCompileHintsPerFunctionMagicComment == 32);
  static_assert(static_cast<int>(v8::ScriptType::kClassic) == 0);
  static_assert(static_cast<int>(v8::ScriptType::kModule) == 1);
  static_assert(static_cast<int>(v8::EmbedderStateTag::EMPTY) == 0);
  static_assert(static_cast<int>(v8::EmbedderStateTag::OTHER) == 1);

  v8::V8::InitializeICUDefaultLocation(nullptr);
  v8::V8::Initialize();

  std::unique_ptr<v8::ArrayBuffer::Allocator> allocator(
      v8::ArrayBuffer::Allocator::NewDefaultAllocator());
  v8::Isolate::CreateParams params;
  params.array_buffer_allocator = allocator.get();
  v8::Isolate* isolate = v8::Isolate::New(params);

  {
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope scope(isolate);
    v8::Local<v8::Context> context = v8::Context::New(isolate);
    v8::Context::Scope context_scope(context);

    v8::Local<v8::Primitive> undefined = v8::Undefined(isolate);
    v8::Local<v8::Primitive> null = v8::Null(isolate);
    v8::Local<v8::Boolean> yes = v8::True(isolate);
    v8::Local<v8::Boolean> no = v8::False(isolate);

    Check(undefined->IsUndefined(), "Undefined() answers an undefined value");
    Check(null->IsNull(), "Null() answers a null value");
    Check(undefined->IsNullOrUndefined(), "an undefined value is null-or-undefined");
    Check(null->IsNullOrUndefined(), "a null value is null-or-undefined");
    Check(yes->IsBoolean() && no->IsBoolean(), "True()/False() answer booleans");

    // The hierarchy, as a compile-time statement: `Boolean` is a `Primitive`
    // in V8 15, and this assignment is what says so here.
    v8::Local<v8::Primitive> primitive_boolean = yes;
    Check(primitive_boolean->IsBoolean(), "a Boolean is a Primitive");

    // Identity: the engine's own singleton, compared by a script against the
    // literal it stands for. A copy or a lookalike fails this.
    v8::Local<v8::Object> global = context->Global();
    global->Set(context, v8::String::NewFromUtf8Literal(isolate, "u"), undefined)
        .ToChecked();
    global->Set(context, v8::String::NewFromUtf8Literal(isolate, "n"), null)
        .ToChecked();
    global->Set(context, v8::String::NewFromUtf8Literal(isolate, "t"), yes)
        .ToChecked();
    global->Set(context, v8::String::NewFromUtf8Literal(isolate, "f"), no)
        .ToChecked();

    Check(RunInt(isolate, context, "globalThis.u === undefined ? 1 : 0") == 1,
          "Undefined(isolate) is the engine's undefined");
    Check(RunInt(isolate, context, "globalThis.n === null ? 1 : 0") == 1,
          "Null(isolate) is the engine's null");
    Check(RunInt(isolate, context, "globalThis.t === true ? 1 : 0") == 1,
          "True(isolate) is the engine's true");
    Check(RunInt(isolate, context, "globalThis.f === false ? 1 : 0") == 1,
          "False(isolate) is the engine's false");

    // `Maybe`'s helpers, which V8 15 spells `Just`/`Nothing`.
    v8::Maybe<int> just = v8::Just(4);
    Check(!just.IsNothing() && just.FromJust() == 4, "Just(4) carries 4");
    v8::Maybe<int> nothing = v8::Nothing<int>();
    Check(nothing.IsNothing(), "Nothing<int>() is empty");

    // The numeric reads and the integer factories Blink's converters call
    // (`Number::New`, `Integer::New`, `Integer::NewFromUnsigned`).
    v8::Local<v8::Number> number = v8::Number::New(isolate, 3.5);
    Check(number->Value() == 3.5, "Number::Value() reads the double");
    v8::Local<v8::Integer> negative = v8::Integer::New(isolate, -7);
    Check(negative->Value() == -7, "Integer::New(-7) reads back -7");
    v8::Local<v8::Integer> big =
        v8::Integer::NewFromUnsigned(isolate, 4000000000u);
    Check(big->Value() == 4000000000LL,
          "NewFromUnsigned keeps the unsigned magnitude");

    // The two narrowed views, and the wrap the language defines for a value
    // outside Int32's range.
    v8::Local<v8::Int32> signed_view = negative.As<v8::Int32>();
    Check(signed_view->Value() == -7, "Int32::Value() on a negative integer");
    v8::Local<v8::Uint32> unsigned_view = big.As<v8::Uint32>();
    Check(unsigned_view->Value() == 4000000000u,
          "Uint32::Value() keeps the magnitude");
    v8::Local<v8::Integer> wide =
        v8::Integer::NewFromUnsigned(isolate, 3000000000u);
    Check(wide.As<v8::Int32>()->Value() == -1294967296,
          "Int32::Value() wraps as ToInt32, not as a raw C++ cast");

    // `ArrayBufferBase`: the type a converter takes when it does not care
    // which buffer kind it was handed (Blink's `ThrowIfResizable`).
    v8::Local<v8::ArrayBuffer> buffer = v8::ArrayBuffer::New(isolate, 64);
    v8::Local<v8::ArrayBufferBase> buffer_base = buffer;
    Check(buffer_base->ByteLength() == 64, "ArrayBufferBase::ByteLength()");
    v8::Local<v8::SharedArrayBuffer> shared =
        v8::SharedArrayBuffer::New(isolate, 32);
    v8::Local<v8::ArrayBufferBase> shared_base = shared;
    Check(shared_base->ByteLength() == 32,
          "a SharedArrayBuffer is an ArrayBufferBase too");

    // The flags are advisory: calling the parser must be harmless.
    v8::V8::SetFlagsFromString("--harmony");

    // BigInt: the three factories, the two 64-bit reads with their lossless
    // flag, and the word array in both directions.
    v8::Local<v8::BigInt> small = v8::BigInt::New(isolate, -5);
    bool lossless = false;
    Check(small->Int64Value(&lossless) == -5 && lossless,
          "BigInt::New(-5) reads back exactly");
    lossless = true;
    Check(small->Uint64Value(&lossless) == 18446744073709551611ull && !lossless,
          "a negative BigInt wraps and says the read was not lossless");

    v8::Local<v8::BigInt> huge =
        v8::BigInt::NewFromUnsigned(isolate, 18446744073709551615ull);
    lossless = false;
    Check(huge->Uint64Value(&lossless) == 18446744073709551615ull && lossless,
          "NewFromUnsigned(2^64-1) is exact");
    lossless = true;
    Check(huge->Int64Value(&lossless) == -1 && !lossless,
          "the same value as an i64 wraps and is not lossless");

    // Two words in, the same two out, sign included: -(2^64 + 3).
    const uint64_t words[2] = {3, 1};
    v8::Local<v8::BigInt> two_words =
        v8::BigInt::NewFromWords(context, 1, 2, words).ToLocalChecked();
    Check(two_words->WordCount() == 2, "NewFromWords keeps two words");
    int sign = 0;
    int count = 2;
    uint64_t out[2] = {0, 0};
    two_words->ToWordsArray(&sign, &count, out);
    Check(sign == 1 && count == 2 && out[0] == 3 && out[1] == 1,
          "ToWordsArray answers the words NewFromWords took");
    Check(two_words->Int64Value(nullptr) == -3,
          "the i64 read sees the two's-complement low word");

    // And the value is the engine's bigint, not a number in disguise.
    global->Set(context, v8::String::NewFromUtf8Literal(isolate, "b"), two_words)
        .ToChecked();
    Check(RunInt(isolate, context,
                 "typeof globalThis.b === 'bigint' ? 1 : 0") == 1,
          "BigInt::NewFromWords makes the engine's own bigint");
  }

  v8::V8::Dispose();
  if (g_failures != 0) {
    fprintf(stderr, "value-basics: %d failure(s)\n", g_failures);
    return 1;
  }
  printf("value-basics: ok\n");
  return 0;
}
