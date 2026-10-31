#ifndef INCLUDE_V8_H_
#define INCLUDE_V8_H_

/*
 * A1's minimal v8.h. It is not a copy of V8's headers: it carries the subset
 * the sample programs and the gin-shaped harness use, with the same names and
 * shapes. `Local<T>` is the word-sized indirect handle from V8's
 * `v8-handle-base.h`; the real header copy is the next milestone
 * (.notes/chromium-facade.md section 4).
 */

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <type_traits>
#include <vector>

#include "slag_a0.h"

namespace v8 {

class Isolate;
class Context;
class ObjectTemplate;
class FunctionTemplate;
class Value;
class String;
class Object;
class Script;
class Message;
class TryCatch;
class ArrayBuffer;

template <class T>
class FunctionCallbackInfo;
template <class T>
class ReturnValue;
template <class T>
class Local;
template <class T>
class Global;
template <class T>
class MaybeLocal;
template <class T>
class Maybe;

enum class NewStringType { kNormal, kInternalized };

class Platform {
 public:
  virtual ~Platform() = default;
};

namespace platform {
std::unique_ptr<Platform> NewDefaultPlatform();
bool PumpMessageLoop(Platform* platform, Isolate* isolate);
}  // namespace platform

class V8 {
 public:
  static bool InitializeICUDefaultLocation(const char* exec_path);
  static void InitializeExternalStartupData(const char* directory);
  static void InitializePlatform(Platform* platform);
  static void Initialize();
  static void Dispose();
  static void DisposePlatform();
  static void SetFlagsFromCommandLine(int* argc, char** argv, bool remove_flags);
  static const char* GetVersion();
};

class ArrayBuffer {
 public:
  class Allocator {
   public:
    virtual ~Allocator() = default;
    static Allocator* NewDefaultAllocator();
  };
};

/*
 * The current execution state. The samples create one isolate and enter one
 * context at a time; A0 keeps that state thread-local rather than in a scope
 * chain.
 */
inline thread_local uint64_t g_current_context = 0;
inline thread_local Isolate* g_current_isolate = nullptr;

using Address = uint64_t;

template <class T>
class Local {
 public:
  Local() = default;
  Local(std::nullptr_t) {}

  template <class S, class = std::enable_if_t<std::is_base_of_v<T, S>>>
  Local(const Local<S>& other) : location_(other.slot()) {}

  bool IsEmpty() const { return location_ == nullptr; }

  template <class S>
  Local<S> As() const {
    return Local<S>::FromSlot(location_);
  }

  T* operator->() const {
    return reinterpret_cast<T*>(static_cast<uintptr_t>(raw()));
  }
  T* operator*() const { return operator->(); }

  // The tagged word this handle names. Only meaningful on a non-empty handle.
  uint64_t raw() const { return location_ != nullptr ? *location_ : 0; }

  // A handle for `value`, allocated in the current handle scope.
  static Local<T> New(uint64_t value);

 private:
  explicit Local(Address* location) : location_(location) {}

  static Local<T> FromSlot(Address* location) {
    return Local<T>(location);
  }

  Address* slot() const { return location_; }

  Address* location_ = nullptr;

  template <class>
  friend class Local;
  template <class>
  friend class Global;
  template <class>
  friend class MaybeLocal;
  template <class>
  friend class FunctionCallbackInfo;
  friend class HandleScope;
};

template <class T>
class Maybe {
 public:
  Maybe() = default;
  explicit Maybe(T value) : value_(value), has_(true) {}

  bool IsNothing() const { return !has_; }
  T FromMaybe(T fallback) const { return has_ ? value_ : fallback; }
  T FromJust() const { return value_; }
  T ToChecked() const { return value_; }

 private:
  T value_{};
  bool has_ = false;
};

template <class T>
class MaybeLocal {
 public:
  MaybeLocal() = default;
  MaybeLocal(Local<T> local) : local_(local) {}

  bool IsEmpty() const { return local_.IsEmpty(); }

  bool ToLocal(Local<T>* out) const {
    if (local_.IsEmpty()) {
      *out = Local<T>();
      return false;
    }
    *out = local_;
    return true;
  }

  Local<T> ToLocalChecked() const {
    if (local_.IsEmpty()) std::abort();
    return local_;
  }

 private:
  Local<T> local_;
};

class Value {
 public:
  bool IsUndefined() const;
  bool IsString() const;
  Maybe<uint32_t> Uint32Value(Local<Context> context) const;
  Maybe<int32_t> Int32Value(Local<Context> context) const;
};

class Object : public Value {};

class Number : public Value {
 public:
  static Local<Number> New(Isolate* isolate, double value);
};

class Integer : public Number {
 public:
  static Local<Integer> New(Isolate* isolate, int32_t value);
};

class String : public Value {
 public:
  static Local<String> NewFromUtf8Literal(Isolate* isolate, const char* literal);
  static MaybeLocal<String> NewFromUtf8(
      Isolate* isolate, const char* str,
      NewStringType type = NewStringType::kNormal, int length = -1);

  int Length() const;

  class Utf8Value {
   public:
    template <class S>
    Utf8Value(Isolate*, Local<S> value) : Utf8Value(value.raw()) {}
    ~Utf8Value() { delete[] data_; }

    char* operator*() const { return data_; }
    explicit operator bool() const { return data_ != nullptr; }

   private:
    explicit Utf8Value(uint64_t value);

    char* data_ = nullptr;
  };
};

class ScriptOrigin {
 public:
  explicit ScriptOrigin(Local<Value> name) : name_(name) {}
  Local<Value> ResourceName() const { return name_; }

 private:
  Local<Value> name_;
};

class Message {
 public:
  bool IsEmpty() const { return true; }
  ScriptOrigin GetScriptOrigin() const { return ScriptOrigin(Local<Value>()); }
  Maybe<int> GetLineNumber(Local<Context>) const { return Maybe<int>(); }
  MaybeLocal<String> GetSourceLine(Local<Context>) const {
    return MaybeLocal<String>();
  }
  Maybe<int> GetStartColumn(Local<Context>) const { return Maybe<int>(); }
  Maybe<int> GetEndColumn(Local<Context>) const { return Maybe<int>(); }
};

class Script {
 public:
  static MaybeLocal<Script> Compile(Local<Context> context,
                                    Local<String> source,
                                    ScriptOrigin* origin = nullptr);
  MaybeLocal<Value> Run(Local<Context> context);
};

class TryCatch {
 public:
  explicit TryCatch(Isolate* isolate) : isolate_(isolate) {}

  bool HasCaught() const;
  Local<Value> Exception() const;
  Local<v8::Message> Message() const { return Local<v8::Message>(); }
  MaybeLocal<Value> StackTrace(Local<Context>) const {
    return MaybeLocal<Value>();
  }

 private:
  Isolate* isolate_;
};

template <class T>
class ReturnValue {
 public:
  explicit ReturnValue(uint64_t* slot) : slot_(slot) {}
  void Set(Local<T> value) { *slot_ = value.raw(); }

 private:
  uint64_t* slot_;
};

template <class T>
class FunctionCallbackInfo {
 public:
  FunctionCallbackInfo(Isolate* isolate, const void* call,
                       const uint64_t* args, int length)
      : isolate_(isolate),
        call_(call),
        args_(args),
        length_(length),
        return_value_(slag_undefined()) {}

  Isolate* GetIsolate() const { return isolate_; }
  int Length() const { return length_; }
  Local<T> operator[](int index) const { return Local<T>::New(args_[index]); }
  ReturnValue<T> GetReturnValue() const {
    return ReturnValue<T>(&return_value_);
  }
  const void* call() const { return call_; }
  uint64_t return_value() const { return return_value_; }

 private:
  Isolate* isolate_;
  const void* call_;
  const uint64_t* args_;
  int length_;
  mutable uint64_t return_value_;
};

using FunctionCallback = void (*)(const FunctionCallbackInfo<Value>&);

class FunctionTemplate {
 public:
  static Local<FunctionTemplate> New(Isolate* isolate,
                                     FunctionCallback callback);
  FunctionCallback callback() const { return callback_; }

 private:
  explicit FunctionTemplate(FunctionCallback callback) : callback_(callback) {}

  FunctionCallback callback_;
};

class ObjectTemplate {
 public:
  struct Entry {
    std::string name;
    FunctionTemplate* function;
  };

  static Local<ObjectTemplate> New(Isolate* isolate);
  void Set(Isolate* isolate, const char* name,
           Local<FunctionTemplate> function);
  const std::vector<Entry>& entries() const { return entries_; }

 private:
  std::vector<Entry> entries_;
};

class Context {
 public:
  static Local<Context> New(
      Isolate* isolate, std::nullptr_t = nullptr,
      Local<ObjectTemplate> global_template = Local<ObjectTemplate>());
  ~Context();

  Local<Object> Global() const;

  /* The A0 C ABI context behind this facade object. */
  SlagContext* slag() const { return slag_; }

  class Scope {
   public:
    explicit Scope(Local<Context> context) : previous_(g_current_context) {
      g_current_context = context.raw();
    }
    ~Scope() { g_current_context = previous_; }

   private:
    uint64_t previous_;
  };

 private:
  SlagContext* slag_ = nullptr;

  friend class Isolate;
};

class Isolate {
 public:
  struct CreateParams {
    ArrayBuffer::Allocator* array_buffer_allocator = nullptr;
  };

  static Isolate* New(const CreateParams& params);
  void Dispose();
  void ThrowError(const char* message);
  Local<Context> GetCurrentContext();

  class Scope {
   public:
    explicit Scope(Isolate* isolate) : previous_(g_current_isolate) {
      g_current_isolate = isolate;
    }
    ~Scope() { g_current_isolate = previous_; }

   private:
    Isolate* previous_;
  };

 private:
  Isolate() = default;
  ~Isolate() = default;

  SlagIsolate* slag_ = nullptr;
  std::vector<Context*> contexts_;

  friend class Context;
};

/*
 * A thread-local stack of handle slots, owned by facade.cc. A `Local<T>` is a
 * pointer into it, the word-sized indirect handle V8's `v8-handle-base.h`
 * describes. Keeping the stack out of line gives the facade and its host one
 * stack, not one per translation unit.
 */
class HandleScope {
 public:
  explicit HandleScope(Isolate* isolate);
  ~HandleScope();

  HandleScope(const HandleScope&) = delete;
  HandleScope& operator=(const HandleScope&) = delete;

  static Address* CreateHandle(uint64_t value);

 private:
  std::size_t mark_;
};

template <class T>
Local<T> Local<T>::New(uint64_t value) {
  return Local<T>(HandleScope::CreateHandle(value));
}

template <class T>
class Global {
 public:
  Global() = default;
  Global(Isolate*, Local<T> local)
      : value_(local.IsEmpty() ? 0 : local.raw()), empty_(local.IsEmpty()) {}

  bool IsEmpty() const { return empty_; }
  Local<T> Get(Isolate*) const {
    return empty_ ? Local<T>() : Local<T>::New(value_);
  }

 private:
  uint64_t value_ = 0;
  bool empty_ = true;
};

/* The tagged word behind a `this` pointer: API objects have no fields. */
inline uint64_t Raw(const void* self) {
  return static_cast<uint64_t>(reinterpret_cast<uintptr_t>(self));
}

}  // namespace v8

#endif  // INCLUDE_V8_H_
