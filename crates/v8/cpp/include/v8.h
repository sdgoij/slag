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
#include <deque>
#include <memory>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <vector>

#include "slag_a0.h"
#include "v8-version.h"

namespace v8 {

class Isolate;
class Context;
class ObjectTemplate;
class FunctionTemplate;
class Value;
class String;
class Name;
class Object;
class Script;
class Message;
class TryCatch;
class ArrayBuffer;
class Private;
class Exception;
class Function;
class Promise;

template <class T>
class FunctionCallbackInfo;
template <class T>
class PropertyCallbackInfo;
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

class AccessorSignature;
class Signature;
class FunctionTemplate;
class ObjectTemplate;
class Name;
class Private;
class ArrayBuffer;
class Value;
class Context;
class Isolate;
class Object;
class Script;

enum class NewStringType { kNormal, kInternalized };

/* How `String::Write` behaves about the trailing NUL (`v8::WriteOptions`). The
 * facade honours `NO_NULL_TERMINATION` and treats the rest as the default. */
enum WriteOptions {
  NO_OPTIONS = 0,
  HINT_MANY_WRITES_EXPECTED = 1,
  NO_NULL_TERMINATION = 2,
  PRESERVE_ONE_BYTE_NULL = 4,
  REPLACE_INVALID_UTF8 = 8
};

/* When the job queues drain (`v8::MicrotasksPolicy`).
 *
 * `kAuto`, the default, drains after a successful `Script::Run` returns, so the
 * run's own microtasks have settled by the time the host sees the result.
 * `kExplicit` leaves them queued until the host calls
 * `Isolate::PerformMicrotaskCheckpoint` or pumps the message loop. `kScoped` is
 * accepted and behaves like `kExplicit`: the facade has no `MicrotasksScope` to
 * close. */
enum class MicrotasksPolicy { kExplicit = 0, kScoped = 1, kAuto = 2 };

/* A host-visible microtask queue (`v8::MicrotaskQueue`).
 *
 * V8 15 derives this from `cppgc::GarbageCollected` — the collector frees a
 * queue no context references — so the faithful shape waits for cppgc
 * (`.notes/browser.md` §5). This one is an ordinary object the embedder owns:
 * `New` hands out a queue, its destructor releases the engine's queue, and a
 * `Context` deletes the one it made for itself. That is V8's own ownership rule
 * stated in `v8-microtask-queue.h`: the embedder keeps the queue alive until the
 * contexts using it are gone.
 *
 * A queue attached to a context (`Context::New`'s last argument) takes that
 * realm's promise jobs, so only `PerformCheckpoint` runs them — which is what
 * Blink's `bindings/core/v8` does with the pointer it gets from
 * `Context::GetMicrotaskQueue()`.
 *
 * Not here yet: the two `EnqueueMicrotask` overloads. A host function queued as
 * a microtask must stay rooted until it runs, and the engine's job queue holds
 * closures it cannot trace (`.notes/barrier-uaf.md`), so that needs a rooted
 * host-job entry point rather than a wrapper. */
class MicrotaskQueue {
 public:
  using MicrotasksCompletedCallbackWithData = void (*)(Isolate*, void*);

  /* The queue is created before any context exists in the order a host uses
   * (Blink builds its isolate's queue, then its contexts), so the engine id is
   * minted when a context binds it (`Context::New`). */
  static MicrotaskQueue* New(Isolate* isolate,
                             MicrotasksPolicy policy = MicrotasksPolicy::kAuto);
  virtual ~MicrotaskQueue();

  /* Fires after every checkpoint, including one that ran nothing — V8's
   * contract, and what a host uses to detect an empty drain. */
  virtual void AddMicrotasksCompletedCallback(
      MicrotasksCompletedCallbackWithData callback, void* data = nullptr);
  virtual void RemoveMicrotasksCompletedCallback(
      MicrotasksCompletedCallbackWithData callback, void* data = nullptr);

  virtual void PerformCheckpoint(Isolate* isolate);
  virtual bool IsRunningMicrotasks() const { return running_; }
  virtual int GetMicrotasksScopeDepth() const { return scope_depth_; }

  /* The policy the queue was created with. This engine reads a queue's policy
   * once, at creation, so setting it later is recorded here and not passed on. */
  virtual void SetMicrotasksPolicy(MicrotasksPolicy policy) { policy_ = policy; }
  virtual MicrotasksPolicy GetMicrotasksPolicy() const { return policy_; }

  MicrotaskQueue(const MicrotaskQueue&) = delete;
  MicrotaskQueue& operator=(const MicrotaskQueue&) = delete;

 protected:
  explicit MicrotaskQueue(int32_t id, MicrotasksPolicy policy)
      : id_(id), policy_(policy) {}

 private:
  int32_t id_ = -1;
  MicrotasksPolicy policy_ = MicrotasksPolicy::kAuto;
  bool running_ = false;
  int scope_depth_ = 0;
  std::vector<std::pair<MicrotasksCompletedCallbackWithData, void*>> completed_;

  /* The id is the engine's handle on this queue: a `Context` is what attaches
   * it, and the facade's ABI takes it, so both read it directly. */
  friend class Context;
};

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
  /* V8's own flag parser. The facade holds no flags, so this accepts the
   * string and does nothing, like `SetFlagsFromCommandLine` above; a host that
   * passes `--jitless` to a facade that has no such switch is not misled. */
  static void SetFlagsFromString(const char* str);
  static const char* GetVersion();
};

/* `ArrayBuffer` derives from `Object`, so it is defined below; only pointers
 * to it appear before that point (`Isolate::CreateParams`). */

/*
 * The current execution state. The samples create one isolate and enter one
 * context at a time; A0 keeps that state thread-local rather than in a scope
 * chain.
 */
inline thread_local uint64_t g_current_context = 0;
/* How many `TryCatch`es are alive on this thread. V8 reports an error to the
 * message listeners only when nothing is catching it, and the inspector's
 * `Runtime.exceptionThrown` rides on that. */
inline thread_local int g_try_catch_depth = 0;
inline thread_local Isolate* g_current_isolate = nullptr;

using Address = uint64_t;

/* One handle slot. `rooted` says whether `value` is an engine value the C ABI
 * must retain; facade-owned pointers (Context, Script, templates) are not. */
struct HandleSlot {
  Address value;
  bool rooted;
};

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

  /* The same value, which is how V8's handles compare: two handles over one
   * value are equal, and an empty handle equals only another empty one. */
  template <class S>
  bool operator==(const Local<S>& that) const {
    return raw() == that.raw();
  }
  template <class S>
  bool operator!=(const Local<S>& that) const {
    return !(*this == that);
  }

  T* operator->() const {
    return reinterpret_cast<T*>(static_cast<uintptr_t>(raw()));
  }
  T* operator*() const { return operator->(); }

  // The tagged word this handle names. Only meaningful on a non-empty handle.
  uint64_t raw() const { return location_ != nullptr ? location_->value : 0; }

  // A handle for `value`, allocated in the current handle scope.
  static Local<T> New(uint64_t value);

 private:
  explicit Local(HandleSlot* location) : location_(location) {}

  static Local<T> FromSlot(HandleSlot* location) {
    return Local<T>(location);
  }

  HandleSlot* slot() const { return location_; }

  HandleSlot* location_ = nullptr;

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
Maybe<T> Just(T value) {
  return Maybe<T>(value);
}

template <class T>
Maybe<T> Nothing() {
  return Maybe<T>();
}

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

/* The tag a host attaches to a pointer it stores in an internal field
 * (`v8-sandbox.h`). Slag's internal fields are untraced opaque words with no
 * pointer-tag check, so the enumerators exist so a host that passes one
 * compiles, and their values are placeholders rather than V8's. */
enum class CppHeapPointerTag : uint16_t {
  kFirstTag = 0,
  kDefaultTag = kFirstTag,
  kEmbedderObjectWrapperTag = 1,
  kLastTag = kEmbedderObjectWrapperTag,
  kAnyCppHeapPointer = 0xffff,
};

/* Whether a property-handler callback answered, and how (`v8::Intercepted`).
 * `kNo` runs the ordinary internal method. */
enum class Intercepted {
  kNo = 0,
  kYes = 1,
  kYesKeepExisting = 2,
  kThrow = 3,
};

/* The attributes a property is installed with (`v8::PropertyAttribute`). The
 * facade honours `DontEnum` and `DontDelete` on an accessor and treats
 * `ReadOnly` as "no setter"; the template forms carry the same bits. */
enum PropertyAttribute {
  None = 0,
  ReadOnly = 1 << 0,
  DontEnum = 1 << 1,
  DontDelete = 1 << 2,
};

class Value {
 public:
  /* The type predicates (`v8::Value::IsArray` and its siblings). The shape bits
   * come from the value; the brands the engine records per object (date,
   * regexp, map, set, promise, native error, array buffer) need a live agent, so
   * they answer false outside one. */
  bool IsUndefined() const;
  bool IsNull() const;
  bool IsNullOrUndefined() const;
  bool IsBoolean() const;
  bool IsNumber() const;
  bool IsInt32() const;
  bool IsUint32() const;
  bool IsBigInt() const;
  bool IsSymbol() const;
  bool IsString() const;
  bool IsFunction() const;
  bool IsObject() const;
  bool IsArray() const;
  bool IsExternal() const;
  bool IsTypedArray() const;
  bool IsProxy() const;
  bool IsArgumentsObject() const;
  bool IsDate() const;
  bool IsRegExp() const;
  bool IsPromise() const;
  bool IsMap() const;
  bool IsSet() const;
  bool IsArrayBuffer() const;
  bool IsSharedArrayBuffer() const;
  bool IsDataView() const;
  bool IsNativeError() const;

  /* ToBoolean, `typeof`, and the coercions. `ToString`/`ToNumber`/`ToObject`
   * run host code, so they need the context; an empty answer leaves the error
   * pending. */
  bool BooleanValue(Isolate* isolate) const;
  MaybeLocal<String> TypeOf(Local<Context> context) const;
  MaybeLocal<String> ToString(Local<Context> context) const;
  Maybe<double> ToNumber(Local<Context> context) const;
  MaybeLocal<Object> ToObject(Local<Context> context) const;
  bool StrictEquals(Local<Value> that) const;
  bool SameValue(Local<Value> that) const;

  /* The integer conversions. Each runs ToNumber first, so an object's `valueOf`
   * is what a host sees, and each wraps rather than failing on a converted
   * number that does not fit, which is what ToInt32/ToUint32 do. An empty
   * answer means the conversion threw and the error is pending. */
  Maybe<uint32_t> Uint32Value(Local<Context> context) const;
  Maybe<int32_t> Int32Value(Local<Context> context) const;
  Maybe<int64_t> IntegerValue(Local<Context> context) const;
};

/* An accessor's callbacks (`v8::AccessorNameGetterCallback` and its setter):
 * the key the accessor was installed under, and the property-callback info the
 * handler callbacks use. */
using AccessorNameGetterCallback = void (*)(Local<Name> property,
                                            const PropertyCallbackInfo<Value>& info);
using AccessorNameSetterCallback = void (*)(Local<Name> property, Local<Value> value,
                                            const PropertyCallbackInfo<void>& info);

class Object : public Value {
 public:
  /* The internal field `Wrappable<T>` reads: the first, where gin keeps the
   * wrapper information. V8 hides this field index behind its own constant; the
   * facade names it because it is the convention between these two sides. */
  static constexpr int kWrappableFieldIndex = 0;

  /* An ordinary object (`v8::Object::New`). Inside a property-handler callback
   * this allocates through the running callback, which is what lets a getter
   * answer with an object it just built. */
  static Local<Object> New(Isolate* isolate);
  static Local<Object> New(Local<Context> context);

  int InternalFieldCount() const;
  bool IsApiWrapper() const;

  /* Store the host's pointer in one internal field
   * (`Object::SetAlignedPointerInInternalField`). V8 aborts on an index past the
   * count; the facade records the ABI's refusal instead, and `tag` is not
   * inspected. */
  void SetAlignedPointerInInternalField(
      int index, void* value,
      CppHeapPointerTag tag = CppHeapPointerTag::kDefaultTag);

  /* Whether the object is callable (`Object::IsCallable`) or a constructor
   * (`Object::IsConstructor`). */
  bool IsCallable() const;
  bool IsConstructor() const;

  void* GetAlignedPointerFromInternalField(int index) const;
  void* GetAlignedPointerFromInternalField(Isolate* isolate, int index) const;
  void* GetAlignedPointerFromInternalField(Local<Context> context,
                                           int index) const;

  /* [[Get]] and [[Set]] by a key value (`Object::Get`/`Object::Set`). The key
   * goes through the engine's ToPropertyKey, so a number or an object key names
   * the property the language would. */
  MaybeLocal<Value> Get(Local<Context> context, Local<Value> key);
  Maybe<bool> Set(Local<Context> context, Local<Value> key, Local<Value> value);

  /* The index forms. An index is a property key spelled as a number, so these
   * are the key-value forms with the number built for the caller. */
  MaybeLocal<Value> Get(Local<Context> context, uint32_t index);
  Maybe<bool> Set(Local<Context> context, uint32_t index, Local<Value> value);

  /* [[HasProperty]]: the own property, then the chain (`Object::Has`). */
  Maybe<bool> Has(Local<Context> context, Local<Value> key);
  Maybe<bool> Has(Local<Context> context, uint32_t index);

  /* [[Delete]]: `true` once the property is gone, including when it was never
   * there (`Object::Delete`). */
  bool Delete(Local<Context> context, Local<Value> key);
  bool Delete(Local<Context> context, uint32_t index);

  /* The object's own named property, with no interceptor and no prototype walk,
   * and an own accessor's getter run on this object
   * (`Object::GetRealNamedProperty`). Empty when the object has no such own
   * property. */
  MaybeLocal<Value> GetRealNamedProperty(Local<Context> context, Local<Name> key);

  /* The object's identity hash (`Object::GetIdentityHash`): stable for the
   * object's life, and equal for handles that compare equal. */
  int GetIdentityHash() const;

  /* Install an accessor on this object (`Object::SetAccessor`): a read of `name`
   * runs `getter` and a write runs `setter`. The specification is owned by the
   * engine for the object's lifetime, like a template. `attribute` carries
   * V8's `DontEnum`/`DontDelete` bits; `ReadOnly` installs no setter. */
  void SetAccessor(Local<Context> context, Local<Name> name,
                   AccessorNameGetterCallback getter,
                   AccessorNameSetterCallback setter = nullptr,
                   Local<Value> data = Local<Value>(),
                   PropertyAttribute attribute = None);
  /* The same, declared over a receiver type (`v8::AccessorSignature`). */
  void SetAccessor(Local<Context> context, Local<Name> name,
                   AccessorNameGetterCallback getter,
                   AccessorNameSetterCallback setter, Local<Value> data,
                   Local<AccessorSignature> signature,
                   PropertyAttribute attribute = None);

  /* The context this object was created in (`Object::GetCreationContext`). The
   * facade has one realm per entered context and the engine does not record a
   * holder's realm on each object, so the entered context is the answer; with
   * none entered the handle is empty. */
  MaybeLocal<Context> GetCreationContext() const;
  Local<Context> GetCreationContextChecked() const;

  /* Install an accessor whose get and set are FunctionTemplates
   * (`Object::SetAccessorProperty`): each runs with the receiver as `this`. */
  void SetAccessorProperty(Local<Name> name, Local<FunctionTemplate> getter,
                           Local<FunctionTemplate> setter,
                           PropertyAttribute attribute = None);

  /* Private names (`Object::GetPrivate` and its siblings). A private name is
   * the symbol `v8::Private` minted, so the ordinary key paths reach it. */
  MaybeLocal<Value> GetPrivate(Local<Context> context, Local<Private> key);
  Maybe<bool> SetPrivate(Local<Context> context, Local<Private> key,
                         Local<Value> value);
  Maybe<bool> HasPrivate(Local<Context> context, Local<Private> key);
  bool DeletePrivate(Local<Context> context, Local<Private> key);

  /* The own-property predicates (`Object::HasRealNamedProperty` and its
   * siblings): no interceptor and no prototype walk. `HasRealIndexedProperty`
   * asks about an array index; the other two about a name. */
  Maybe<bool> HasRealNamedProperty(Local<Context> context, Local<Name> key);
  Maybe<bool> HasRealIndexedProperty(Local<Context> context, uint32_t index);
  Maybe<bool> HasRealNamedCallbackProperty(Local<Context> context,
                                           Local<Name> key);

  /* The object's prototype (`Object::GetPrototype`): `null` when it has none.
   * `SetPrototype` answers whether the engine accepted the change; an empty
   * handle sets a null prototype. */
  Local<Value> GetPrototype();
  bool SetPrototype(Local<Context> context, Local<Value> prototype);

  /* The C++ object this wrapper names. V8 requires its `T` to be a cppgc
   * `GarbageCollectedMixin`; the facade has no cppgc, so there is no such
   * constraint and nothing is checked. */
  template <class T>
  T* Wrappable() const {
    return static_cast<T*>(
        GetAlignedPointerFromInternalField(kWrappableFieldIndex));
  }
};

/* A contiguous run of memory (`v8::MemorySpan`), the shape
 * `WasmModuleObject::Compile` takes. */
template <class T>
class MemorySpan {
 public:
  MemorySpan() = default;
  MemorySpan(T* data, size_t size) : data_(data), size_(size) {}

  T* data() const { return data_; }
  size_t size() const { return size_; }
  T* begin() const { return data_; }
  T* end() const { return data_ + size_; }
  T& operator[](size_t index) const { return data_[index]; }

 private:
  T* data_ = nullptr;
  size_t size_ = 0;
};

/* A decoded WebAssembly module (`v8::CompiledWasmModule`). The engine's compiled
 * form is plain data, so this owns a box the facade frees; it is move-only. */
class CompiledWasmModule {
 public:
  CompiledWasmModule() = default;
  CompiledWasmModule(CompiledWasmModule&& other) noexcept
      : module_(other.module_) {
    other.module_ = nullptr;
  }
  CompiledWasmModule& operator=(CompiledWasmModule&& other) noexcept {
    if (this != &other) {
      Reset();
      module_ = other.module_;
      other.module_ = nullptr;
    }
    return *this;
  }
  ~CompiledWasmModule() { Reset(); }

  CompiledWasmModule(const CompiledWasmModule&) = delete;
  CompiledWasmModule& operator=(const CompiledWasmModule&) = delete;

  bool is_null() const { return module_ == nullptr; }

 private:
  friend class WasmModuleObject;
  explicit CompiledWasmModule(SlagWasmModule* module) : module_(module) {}

  void Reset() {
    if (module_ != nullptr) {
      slag_wasm_module_free(module_);
      module_ = nullptr;
    }
  }

  SlagWasmModule* module_ = nullptr;
};

/* A `WebAssembly.Memory` (`v8::WasmMemoryObject`). */
class WasmMemoryObject : public Object {
 public:
  static bool IsWasmMemoryObject(Local<Value> value);
  /* The memory's current buffer, which the engine materializes after a grow. */
  Local<ArrayBuffer> Buffer();
  size_t Size() const;
};

/* A `WebAssembly.Instance` (`v8::WasmInstanceObject`): the engine's exports
 * record is both the brand and the lookup table, so `IsWasmInstanceObject`
 * answers for exactly the objects the rest of this class can read. */
class WasmInstanceObject : public Object {
 public:
  static bool IsWasmInstanceObject(Local<Value> value);

  /* A named or indexed export when it is a function. */
  MaybeLocal<Value> GetExportedFunction(Local<Context> context, Local<Name> name);
  MaybeLocal<Value> GetExportedFunction(Local<Context> context, int index);
  /* The engine's function index for the named export, or -1. */
  int GetFunctionIndex(Local<Name> name);
  /* The first memory the instance exports, or an empty handle. */
  Local<WasmMemoryObject> GetMemoryObject();
};

/* A WebAssembly module object (`v8::WasmModuleObject`): the object
 * `WebAssembly.Module` answers, compiled here or made from a decoded module. */
class WasmModuleObject : public Object {
 public:
  static MaybeLocal<WasmModuleObject> Compile(
      Isolate* isolate, MemorySpan<const uint8_t> wire_bytes);
  static MaybeLocal<WasmModuleObject> FromCompiledModule(
      Isolate* isolate, const CompiledWasmModule& module);

  CompiledWasmModule GetCompiledModule();
};

/* A block of buffer bytes (`v8::BackingStore`). The facade's store is a handle
 * to the engine's refcounted block, so a store taken from a buffer keeps the
 * bytes alive after the object that named them is gone. A new store is owned by
 * the caller (`std::unique_ptr`); a store read off a buffer is shared
 * (`std::shared_ptr`), the ownership V8's modern signatures use. */
class BackingStore {
 public:
  ~BackingStore();

  void* Data() const;
  size_t ByteLength() const;
  bool IsShared() const;
  bool IsEmpty() const { return buffer_ == nullptr; }

 private:
  friend class ArrayBuffer;
  friend class SharedArrayBuffer;

  explicit BackingStore(SlagBuffer* buffer) : buffer_(buffer) {}

  BackingStore(const BackingStore&) = delete;
  BackingStore& operator=(const BackingStore&) = delete;

  SlagBuffer* buffer_ = nullptr;
};

/* How a host releases memory it lent to a backing store
 * (`v8::BackingStoreDeleterCallback`): the bytes, their length, and the host's
 * own context pointer. */
using BackingStoreDeleterCallback = void (*)(void* data, size_t length,
                                             void* deleter_data);

/* A JavaScript ArrayBuffer (`v8::ArrayBuffer`): either a new zeroed buffer, a
 * buffer over a backing store, or the store behind an existing buffer. The
 * host's external-memory account is told about the bytes through
 * `Isolate::AdjustAmountOfExternalAllocatedMemory`, as in V8. */
/* The base `ArrayBuffer` and `SharedArrayBuffer` share in V8 15. A typed
 * array's `GetBuffer()` answers one of these, which is why a converter (Blink's
 * `ThrowIfResizable`) takes a `Local<ArrayBufferBase>`. */
class ArrayBufferBase : public Object {
 public:
  size_t ByteLength() const;
};

class ArrayBuffer : public ArrayBufferBase {
 public:
  /* The host's allocator for buffer bytes (`v8::ArrayBuffer::Allocator`). A
   * backing store the facade creates for an isolate with one of these draws its
   * bytes from it and calls `Free` once the last buffer and store over them are
   * gone, so the host's own accounting (a partition allocator, say) sees them. */
  class Allocator {
   public:
    virtual ~Allocator() = default;
    static Allocator* NewDefaultAllocator();

    /* `length` zeroed bytes, or null when the allocation fails. */
    virtual void* Allocate(size_t length) = 0;
    /* `length` bytes, not zeroed. */
    virtual void* AllocateUninitialized(size_t length);
    virtual void Free(void* data, size_t length) = 0;
    /* A new, larger block whose first `old_length` bytes are copied, or null. */
    virtual void* Reallocate(void* data, size_t old_length, size_t new_length);
  };

  static Local<ArrayBuffer> New(Isolate* isolate, size_t byte_length);
  static Local<ArrayBuffer> New(Isolate* isolate,
                                std::shared_ptr<BackingStore> backing_store);

  static std::unique_ptr<BackingStore> NewBackingStore(Isolate* isolate,
                                                       size_t byte_length);
  /* A store over memory the host owns. `deleter` runs once, when the last
   * buffer and store over the bytes are gone. */
  static std::unique_ptr<BackingStore> NewBackingStore(
      void* data, size_t byte_length, BackingStoreDeleterCallback deleter,
      void* deleter_data);

  void* Data() const;
  std::shared_ptr<BackingStore> GetBackingStore() const;

  /* `Detach` answers false when the buffer is already detached or when `key`
   * does not match its [[ArrayBufferDetachKey]]. */
  bool Detach(Local<Value> key = Local<Value>());
  bool IsDetachable() const;
  bool WasDetached() const;
  void SetDetachKey(Local<Value> key);
};

/* A JavaScript SharedArrayBuffer (`v8::SharedArrayBuffer`). */
class SharedArrayBuffer : public ArrayBufferBase {
 public:
  static Local<SharedArrayBuffer> New(Isolate* isolate, size_t byte_length);
  static Local<SharedArrayBuffer> New(
      Isolate* isolate, std::shared_ptr<BackingStore> backing_store);
  static std::shared_ptr<BackingStore> NewBackingStore(Isolate* isolate,
                                                       size_t byte_length);

  void* Data() const;
  std::shared_ptr<BackingStore> GetBackingStore() const;
};

/* A view over a buffer (`v8::ArrayBufferView`). `Buffer()` is empty for a view
 * over a SharedArrayBuffer, which the `.Buffer()` callers check with
 * `HasBuffer()`. */
class ArrayBufferView : public Object {
 public:
  size_t ByteLength() const;
  size_t ByteOffset() const;
  bool HasBuffer() const;
  Local<ArrayBuffer> Buffer() const;
};

/* A view over a buffer with any element type (`v8::DataView`). The engine
 * validates the geometry, so a range past the end leaves the RangeError pending
 * and answers an empty handle. */
class DataView : public ArrayBufferView {
 public:
  static Local<DataView> New(Local<ArrayBuffer> array_buffer,
                             size_t byte_offset, size_t byte_length);
  static Local<DataView> New(Local<SharedArrayBuffer> shared_array_buffer,
                             size_t byte_offset, size_t byte_length);
};

/* An Integer-Indexed view (`v8::TypedArray`). */
class TypedArray : public ArrayBufferView {
 public:
  size_t Length() const;
};

/* The twelve typed array constructors. Each `New` validates the byte geometry
 * against the buffer in the engine, so a range that does not fit leaves the
 * RangeError (or the detached TypeError) pending in the isolate. */
class Int8Array : public TypedArray {
 public:
  static Local<Int8Array> New(Local<ArrayBuffer> array_buffer,
                              size_t byte_offset, size_t length);
};

class Uint8Array : public TypedArray {
 public:
  static Local<Uint8Array> New(Local<ArrayBuffer> array_buffer,
                               size_t byte_offset, size_t length);
};

class Uint8ClampedArray : public TypedArray {
 public:
  static Local<Uint8ClampedArray> New(Local<ArrayBuffer> array_buffer,
                                      size_t byte_offset, size_t length);
};

class Int16Array : public TypedArray {
 public:
  static Local<Int16Array> New(Local<ArrayBuffer> array_buffer,
                               size_t byte_offset, size_t length);
};

class Uint16Array : public TypedArray {
 public:
  static Local<Uint16Array> New(Local<ArrayBuffer> array_buffer,
                                size_t byte_offset, size_t length);
};

class Int32Array : public TypedArray {
 public:
  static Local<Int32Array> New(Local<ArrayBuffer> array_buffer,
                               size_t byte_offset, size_t length);
};

class Uint32Array : public TypedArray {
 public:
  static Local<Uint32Array> New(Local<ArrayBuffer> array_buffer,
                                size_t byte_offset, size_t length);
};

class Float16Array : public TypedArray {
 public:
  static Local<Float16Array> New(Local<ArrayBuffer> array_buffer,
                                 size_t byte_offset, size_t length);
};

class Float32Array : public TypedArray {
 public:
  static Local<Float32Array> New(Local<ArrayBuffer> array_buffer,
                                 size_t byte_offset, size_t length);
};

class Float64Array : public TypedArray {
 public:
  static Local<Float64Array> New(Local<ArrayBuffer> array_buffer,
                                 size_t byte_offset, size_t length);
};

class BigInt64Array : public TypedArray {
 public:
  static Local<BigInt64Array> New(Local<ArrayBuffer> array_buffer,
                                  size_t byte_offset, size_t length);
};

class BigUint64Array : public TypedArray {
 public:
  static Local<BigUint64Array> New(Local<ArrayBuffer> array_buffer,
                                   size_t byte_offset, size_t length);
};

/* A JavaScript array (`v8::Array`). The element forms are the ones a handler
 * needs: an indexed property handler's enumerator answers with the names it
 * owns, which is an array. */
class Array : public Object {
 public:
  static Local<Array> New(Isolate* isolate, int length = 0);
  static Local<Array> New(Isolate* isolate, Local<Value>* elements,
                          size_t length);
  static Local<Array> New(Local<Context> context, int length = 0);
  static Local<Array> New(Local<Context> context, Local<Value>* elements,
                          size_t length);
};

/* A JavaScript Map (`v8::Map`). The mutators run the engine's own
 * `%Map.prototype.*%` intrinsics and `Size`/`AsArray` read its internal
 * `[[MapData]]`, so a script cannot change what a host's call does. */
class Map : public Object {
 public:
  static Local<Map> New(Isolate* isolate);
  static Local<Map> New(Local<Context> context);

  MaybeLocal<Value> Get(Local<Context> context, Local<Value> key);
  MaybeLocal<Map> Set(Local<Context> context, Local<Value> key,
                      Local<Value> value);
  Maybe<bool> Has(Local<Context> context, Local<Value> key);
  Maybe<bool> Delete(Local<Context> context, Local<Value> key);
  /* The entries flattened to `[key, value, ...]`, as V8 answers. */
  MaybeLocal<Array> AsArray(Local<Context> context);
  /* The live entry count, or -1 when the handle is not a map. */
  int Size(Local<Context> context);
  void Clear();
};

/* A JavaScript Set (`v8::Set`), built the same way as `Map`. */
class Set : public Object {
 public:
  static Local<Set> New(Isolate* isolate);
  static Local<Set> New(Local<Context> context);

  MaybeLocal<Set> Add(Local<Context> context, Local<Value> value);
  Maybe<bool> Has(Local<Context> context, Local<Value> value);
  Maybe<bool> Delete(Local<Context> context, Local<Value> value);
  MaybeLocal<Array> AsArray(Local<Context> context);
  int Size(Local<Context> context);
  void Clear();
};

/* A JavaScript promise (`v8::Promise`). `State`/`Result` read the engine's
 * promise record; `Then`/`Then2`/`Catch` and the resolver go through the
 * engine's own PerformPromiseThen and NewPromiseCapability, so a script that
 * replaced `Promise.prototype.then` cannot change what a host's call does. */
class Promise : public Object {
 public:
  /* A promise's state (`v8::Promise::PromiseState`). */
  enum PromiseState { kPending, kFulfilled, kRejected };

  /* A promise resolved with `value` (`v8::Promise::Resolve`): the value itself
   * when it is already a promise of the realm, and a new fulfilled one
   * otherwise. `Reject` makes a new rejected promise. */
  static MaybeLocal<Promise> Resolve(Local<Context> context, Local<Value> value);
  static MaybeLocal<Promise> Reject(Local<Context> context, Local<Value> value);

  /* The state the engine recorded. A value that is not a promise answers
   * `kPending`, which is what a pending one answers too; V8 does not offer a
   * way to ask the question, and the handle's type already says it. */
  PromiseState State();
  /* The settled value, or an empty handle while pending. */
  Local<Value> Result();
  /* [[IsHandled]], the bookkeeping a host does when it takes a rejection it is
   * not reacting to. */
  Maybe<bool> MarkAsHandled();

  MaybeLocal<Promise> Then(Local<Context> context, Local<Function> handler);
  MaybeLocal<Promise> Then2(Local<Context> context, Local<Function> on_fulfilled,
                            Local<Function> on_rejected);
  MaybeLocal<Promise> Catch(Local<Context> context, Local<Function> handler);

  /* The pair that settles a promise (`v8::Promise::Resolver`). The facade's
   * resolver *is* the pair's resolving function, as in V8, so
   * `GetPromise`/`Reject` name the rest of the capability back through it. */
  class Resolver : public Object {
   public:
    static MaybeLocal<Resolver> New(Local<Context> context);

    Local<Promise> GetPromise();
    Maybe<bool> Resolve(Local<Context> context, Local<Value> value);
    Maybe<bool> Reject(Local<Context> context, Local<Value> value);
  };
};

/* V8 15's intermediate node between `Value` and the value kinds a script can
 * read directly (`.notes/chromium-facade.md` §11, A3). It carries no members;
 * it exists so `Local<Primitive>` and the `Undefined`/`Null` factories below
 * have the type a host's code names. */
class Primitive : public Value {};

/* `Numeric` is the `Primitive` that arithmetic applies to: `Number` (and its
 * `Integer`) in this facade, `BigInt` when it lands. */
class Numeric : public Primitive {};

class Number : public Numeric {
 public:
  double Value() const;
  static Local<Number> New(Isolate* isolate, double value);
};

/* A property key: a `String` or a `Symbol` (`v8::Name`). Nothing here makes a
 * symbol yet, but the type is what `GetRealNamedProperty` takes, and a string
 * is a name, so the hierarchy is the one a host's code assumes. */
class Name : public Primitive {};

class Integer : public Number {
 public:
  static Local<Integer> New(Isolate* isolate, int32_t value);
  static Local<Integer> NewFromUnsigned(Isolate* isolate, uint32_t value);
  int64_t Value() const;
};

/* V8 15's `Int32`/`Uint32`: the `Integer` views a converter narrows to once
 * `IsInt32`/`IsUint32` answered. They have no constructors of their own in V8
 * either — `Integer::New`/`NewFromUnsigned` make them. `Value()` here is the
 * spec's ToInt32/ToUint32 over the stored double, so a value outside the range
 * wraps as the language says instead of being undefined behaviour as a raw
 * C++ cast would be (V8 reaches a `CHECK` where the host broke the contract). */
class Int32 : public Integer {
 public:
  int32_t Value() const;
};

class Uint32 : public Integer {
 public:
  uint32_t Value() const;
};

/* An arbitrary-precision integer (`v8::BigInt`) — the `Numeric` a converter
 * reaches once `IsBigInt` answered. A magnitude crosses as 64-bit words, least
 * significant first, which is V8's own spelling in `NewFromWords`/
 * `ToWordsArray`. No `Cast` here for the same reason the other value kinds have
 * none: the facade has no `v8::Data` to cast from. */
class BigInt : public Numeric {
 public:
  static Local<BigInt> New(Isolate* isolate, int64_t value);
  static Local<BigInt> NewFromUnsigned(Isolate* isolate, uint64_t value);
  /* `(-1)^sign_bit * sum(words[i] * 2^(64 * i))`, words least significant
   * first. Empty when the count is negative or the words are missing. */
  static MaybeLocal<BigInt> NewFromWords(Local<Context> context, int sign_bit,
                                         int word_count, const uint64_t* words);
  /* The low 64 bits, two's complement for a negative value, and through
   * `lossless` whether that is the exact value. */
  uint64_t Uint64Value(bool* lossless = nullptr) const;
  int64_t Int64Value(bool* lossless = nullptr) const;
  int WordCount() const;
  void ToWordsArray(int* sign_bit, int* word_count, uint64_t* words) const;
};

/* The four singletons, as V8 15 spells them: free functions over the isolate's
 * read-only roots rather than `Value` members. `Undefined`/`Null` answer a
 * `Local<Primitive>` and `True`/`False` a `Local<Boolean>`, which is the pair of
 * types Blink's converters pass around. */
Local<Primitive> Undefined(Isolate* isolate);
Local<Primitive> Null(Isolate* isolate);

class String : public Name {
 public:
  static Local<String> NewFromUtf8Literal(Isolate* isolate, const char* literal);
  static MaybeLocal<String> NewFromUtf8(
      Isolate* isolate, const char* str,
      NewStringType type = NewStringType::kNormal, int length = -1);
  /* Latin-1 input: each byte is one code unit, so 0xE9 is U+00E9. */
  static MaybeLocal<String> NewFromOneByte(
      Isolate* isolate, const uint8_t* data,
      NewStringType type = NewStringType::kNormal, int length = -1);
  /* UTF-16 input. The facade copies the code units; there is no external-string
   * store a host handle could keep alive. */
  static MaybeLocal<String> NewFromTwoByte(
      Isolate* isolate, const uint16_t* data,
      NewStringType type = NewStringType::kNormal, int length = -1);

  /* The UTF-16 code unit count, not the UTF-8 byte count (that is
   * `Utf8Length`). */
  int Length() const;
  int Utf8Length(Isolate* isolate) const;
  bool ContainsOnlyOneByte() const;

  /* Write a window of the string into a host buffer. `start` is a code unit
   * index (negative counts from the end), `length` a unit or byte count (-1 for
   * the rest), and the answer is the number written. The buffer must have room
   * for one more unit, because a NUL is appended unless `NO_NULL_TERMINATION`
   * is passed. `WriteUtf8` writes whole characters only and reports their count
   * through `nchars_ref`. */
  int Write(Isolate* isolate, uint16_t* buffer, int start = 0, int length = -1,
            int options = NO_OPTIONS) const;
  int Write(uint16_t* buffer, int start = 0, int length = -1,
            int options = NO_OPTIONS) const;
  int WriteOneByte(Isolate* isolate, uint8_t* buffer, int start = 0,
                   int length = -1, int options = NO_OPTIONS) const;
  int WriteOneByte(uint8_t* buffer, int start = 0, int length = -1,
                   int options = NO_OPTIONS) const;
  int WriteUtf8(Isolate* isolate, char* buffer, int length = -1,
                int* nchars_ref = nullptr,
                int options = NO_OPTIONS) const;

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

/* A boolean value (`v8::Boolean`): the info a deleter answers through. */
class Boolean : public Primitive {
 public:
  static Local<Boolean> New(Isolate* isolate, bool value);
};

Local<Boolean> True(Isolate* isolate);
Local<Boolean> False(Isolate* isolate);

/* A private name (`v8::Private`). V8's `Private` derives from `Data` and is used
 * only through the `Object::*Private` methods; the engine models a private name
 * as a symbol, so this one is a `Value` too and the ordinary key paths reach it
 * (which is the divergence `.notes/embedding.md` records for the Rust bridge).
 * `ForApi` dedupes by name across the isolate; `New` mints a fresh name each
 * call. */
class Private : public Name {
 public:
  static Local<Private> ForApi(Isolate* isolate, Local<String> name);
  static Local<Private> New(Isolate* isolate, Local<String> name);

  /* The name the private was minted with (`Private::Name`), or `undefined`. */
  Local<Value> Name() const;
};

/* Make a built-in error object to throw (`v8::Exception::Error` and its typed
 * siblings). The value is the engine's own constructor result, so a script
 * catches an object with the usual `name`, `message`, and stack. */
class Exception {
 public:
  static Local<Value> Error(Local<String> message);
  static Local<Value> EvalError(Local<String> message);
  static Local<Value> RangeError(Local<String> message);
  static Local<Value> ReferenceError(Local<String> message);
  static Local<Value> SyntaxError(Local<String> message);
  static Local<Value> TypeError(Local<String> message);
  static Local<Value> UriError(Local<String> message);

  /* A message's source position. The engine records one only for a compile
   * error (see `TryCatch::Message`), so an exception made at run time has none
   * and this answers an empty handle. */
  static Local<v8::Message> CreateMessage(Isolate* isolate,
                                          Local<Value> exception);
};

/* A property descriptor (`v8::PropertyDescriptor`): what a definer callback
 * receives. Each field is present-or-absent, because a descriptor distinguishes
 * "not mentioned" from "mentioned and false". */
class PropertyDescriptor {
 public:
  PropertyDescriptor() = default;
  explicit PropertyDescriptor(Local<Value> value)
      : value_(value), has_value_(true), has_writable_(true), writable_(false) {}
  PropertyDescriptor(Local<Value> value, bool writable)
      : value_(value), has_value_(true), has_writable_(true), writable_(writable) {}
  PropertyDescriptor(Local<Value> get, Local<Value> set)
      : get_(get), set_(set), has_get_(true), has_set_(true) {}

  bool has_value() const { return has_value_; }
  Local<Value> value() const { return value_; }
  bool has_writable() const { return has_writable_; }
  bool writable() const { return writable_; }
  bool has_get() const { return has_get_; }
  Local<Value> get() const { return get_; }
  bool has_set() const { return has_set_; }
  Local<Value> set() const { return set_; }

  void set_enumerable(bool enumerable) {
    has_enumerable_ = true;
    enumerable_ = enumerable;
  }
  bool has_enumerable() const { return has_enumerable_; }
  bool enumerable() const { return enumerable_; }

  void set_configurable(bool configurable) {
    has_configurable_ = true;
    configurable_ = configurable;
  }
  bool has_configurable() const { return has_configurable_; }
  bool configurable() const { return configurable_; }

 private:
  Local<Value> value_;
  Local<Value> get_;
  Local<Value> set_;
  bool has_value_ = false;
  bool has_writable_ = false;
  bool writable_ = false;
  bool has_get_ = false;
  bool has_set_ = false;
  bool has_enumerable_ = false;
  bool enumerable_ = false;
  bool has_configurable_ = false;
  bool configurable_ = false;
};

class ScriptOrigin {
 public:
  explicit ScriptOrigin(Local<Value> name) : name_(name) {}
  Local<Value> ResourceName() const { return name_; }

 private:
  Local<Value> name_;
};

/* The details a host prints about a caught exception (`v8::Message`).
 *
 * A message exists only when the engine recorded a position for the error, and
 * it records one only for a failed compile: a runtime error's span may index
 * another script, so a position would be a guess (the same narrowing the Rust
 * bridge documents). `TryCatch::Message()` is therefore empty for a runtime
 * error, and a host prints the exception alone.
 *
 * The three values are roots the message holds: the resource name, the line's
 * text, and the message text. */
class Message {
 public:
  Message(uint64_t resource_name, uint64_t source_line, uint64_t text,
          int line, int start, int end);
  ~Message();

  Message(const Message&) = delete;
  Message& operator=(const Message&) = delete;

  bool IsEmpty() const { return false; }
  Local<String> Get() const { return Local<String>::New(text_); }
  ScriptOrigin GetScriptOrigin() const {
    return ScriptOrigin(Local<Value>::New(resource_name_));
  }
  Maybe<int> GetLineNumber(Local<Context>) const { return Maybe<int>(line_); }
  Maybe<int> GetStartColumn(Local<Context>) const { return Maybe<int>(start_); }
  Maybe<int> GetEndColumn(Local<Context>) const { return Maybe<int>(end_); }
  MaybeLocal<String> GetSourceLine(Local<Context>) const {
    return MaybeLocal<String>(Local<String>::New(source_line_));
  }

 private:
  uint64_t resource_name_;
  uint64_t source_line_;
  uint64_t text_;
  int line_;
  int start_;
  int end_;
};

/* One captured call frame (`v8::StackFrame`): the innermost frame of a stack a
 * host captured with `StackTrace::CurrentStackTrace`. The names are roots, so a
 * frame outlives the scope it was captured in (like `Message`'s strings). */
class StackFrame {
 public:
  /* The 1-based line, 0 when the engine has none. */
  int GetLineNumber() const { return line_; }
  /* The 1-based column, 0 when the engine has none. */
  int GetColumnNumber() const { return column_; }
  /* The script's id, 0 when the code carries none (a classic script has no
   * per-script identity in this engine yet, which the inspector's
   * `Debugger.scriptParsed` needs and has not got). */
  int GetScriptId() const { return static_cast<int>(script_id_); }
  Local<String> GetScriptName() const;
  Local<String> GetScriptNameOrSourceURL() const { return GetScriptName(); }
  Local<String> GetFunctionName() const;

 private:
  friend class StackTrace;
  StackFrame(uint32_t script_id, int line, int column, uint64_t function_name,
             uint64_t script_name);
  ~StackFrame();
  StackFrame(const StackFrame&) = delete;
  StackFrame& operator=(const StackFrame&) = delete;

  uint32_t script_id_;
  int line_;
  int column_;
  uint64_t function_name_;
  uint64_t script_name_;
};

enum StackTraceOptions { kOverview = 0, kDetailed = 1 };

/* The running stack as data (`v8::StackTrace`): what an inspector's
 * `Debugger.paused` and `Runtime.exceptionThrown` carry, where
 * `TryCatch::StackTrace` only answers a rendered string. */
class StackTrace {
 public:
  /* Capture the running stack, innermost first, at most `frame_limit` frames.
   * Empty outside any entered context. */
  static Local<StackTrace> CurrentStackTrace(
      Isolate* isolate, int frame_limit,
      StackTraceOptions options = StackTraceOptions::kOverview);
  int GetFrameCount() const { return static_cast<int>(frames_.size()); }
  Local<StackFrame> GetFrame(uint32_t index) const;
  /* The name of the script the innermost frame runs (`v8::Script`'s own). */
  static Local<String> CurrentScriptNameOrSourceURL(Isolate* isolate) {
    Local<StackTrace> trace = CurrentStackTrace(isolate, 1);
    if (trace.IsEmpty() || trace->GetFrameCount() == 0) {
      return Local<String>();
    }
    return trace->GetFrame(0)->GetScriptName();
  }

 private:
  explicit StackTrace(std::vector<StackFrame*> frames);
  ~StackTrace();
  StackTrace(const StackTrace&) = delete;
  StackTrace& operator=(const StackTrace&) = delete;

  /* Each frame is its own heap object (a `Local<StackFrame>` is a handle over
   * it), so a host can hold one after the trace's own handle is gone. */
  std::vector<StackFrame*> frames_;
};

class Script {
  public:
  static MaybeLocal<Script> Compile(Local<Context> context,
                                    Local<String> source,
                                    ScriptOrigin* origin = nullptr);
  MaybeLocal<Value> Run(Local<Context> context);
};

/* Where an offset in a module's source is (`v8::Location`): a 0-based line and
 * column, which is the shape the Rust bridge's `Location` exposes. */
struct Location {
  int line_number = 0;
  int column_number = 0;

  int GetLineNumber() const { return line_number; }
  int GetColumnNumber() const { return column_number; }
};

/* Where a module is in its life (`v8::ModuleStatus`). */
enum class ModuleStatus {
  kUninstantiated = 0,
  kInstantiating = 1,
  kInstantiated = 2,
  kEvaluating = 3,
  kEvaluated = 4,
  kErrored = 5,
};

class Module;

/* The host's resolver, called for each import a module declares
 * (`v8::ResolveModuleCallback`). Returning an empty handle is a failed link. */
using ResolveModuleCallback = MaybeLocal<Module> (*)(Local<Context> context,
                                                     Local<String> specifier,
                                                     Local<Module> referrer);

/* A module record (`v8::Module`). The facade object is a handle over the
 * engine's pinned record; modules are never freed, like scripts and templates. */
class Module {
 public:
  ModuleStatus GetStatus() const;
  Local<Value> GetException() const;
  Local<Value> GetModuleNamespace();
  int GetModuleRequestsLength() const;
  Local<String> GetModuleRequest(int index) const;
  Local<Integer> GetIdentityHash() const;
  Maybe<bool> IsSourceTextModule() const;
  Maybe<bool> IsSyntheticModule() const;

  /* Link the module and its imports, asking `callback` for each specifier in
   * the graph (`Module::InstantiateModule`). */
  Maybe<bool> InstantiateModule(Local<Context> context,
                                ResolveModuleCallback callback);
  MaybeLocal<Value> Evaluate(Local<Context> context);
  Location SourceOffsetToLocation(int offset) const;
};

/* The source a compile reads (`v8::Source`). Its resource name is the module's
 * specifier and the name a stack frame reports. */
class Source {
 public:
  Source(Local<String> source_string, const ScriptOrigin& origin)
      : source_(source_string), name_(origin.ResourceName()) {}
  explicit Source(Local<String> source_string) : source_(source_string) {}

  Local<String> source_string() const { return source_; }
  Local<Value> resource_name() const { return name_; }

 private:
  Local<String> source_;
  Local<Value> name_;
};

/* Whether a script is a classic script or a module (`v8::ScriptType`). */
enum class ScriptType { kClassic, kModule };

/* A profiler sample's embedder tag (`v8-embedder-state-scope.h`). Only the two
 * values V8 reserves are named; a host defines its own above them. */
enum class EmbedderStateTag : uint8_t {
  EMPTY = 0,
  OTHER = 1,
};

/* The compiler entry points that take a `Source` (`v8::ScriptCompiler`).
 *
 * `CompileOptions` and `NoCacheReason` are nested here because V8 nests them:
 * a host writes `v8::ScriptCompiler::CompileOptions`, and a namespace-scope
 * `v8::CompileOptions` would compile for the facade and fail for Blink.
 * `kConsumeCodeCache` and the reasons are accepted and ignored — this facade
 * has no code cache yet. */
class ScriptCompiler {
 public:
  /* How a compile reads its input. */
  enum CompileOptions {
    kNoCompileOptions = 0,
    kConsumeCodeCache = 1 << 0,
    kEagerCompile = 1 << 1,
    /* The compile-hint family: Blink passes these to say whether the source's
     * magic comments are to be honoured and whether hint data is being
     * produced or consumed. The engine has no hint pipeline, so they are
     * accepted as advisory and have no effect — which is safe, because a hint
     * only tells a compiler about work it may skip. */
    kProduceCompileHints = 1 << 2,
    kConsumeCompileHints = 1 << 3,
    kFollowCompileHintsMagicComment = 1 << 4,
    kFollowCompileHintsPerFunctionMagicComment = 1 << 5,
  };

  enum class NoCacheReason {
    kNoCacheNoReason = 0,
    kNoCacheBecauseCachingDisabled = 1,
  };

  static MaybeLocal<Module> CompileModule(
      Isolate* isolate, Source* source,
      CompileOptions options = kNoCompileOptions,
      NoCacheReason reason = NoCacheReason::kNoCacheNoReason);
};

/* The exceptions a host catches around a compile or a run (`v8::TryCatch`).
 *
 * The committed state is the context's pending error, and the destructor clears
 * it, which is what makes the next operation start clean (V8's TryCatch does
 * the same to the isolate). `Exception`, `Message`, and `StackTrace` all read
 * the same pending error without consuming it. */
class TryCatch {
 public:
  explicit TryCatch(Isolate* isolate) : isolate_(isolate) {
    ++g_try_catch_depth;
  }
  ~TryCatch();

  TryCatch(const TryCatch&) = delete;
  TryCatch& operator=(const TryCatch&) = delete;

  bool HasCaught() const;
  Local<Value> Exception() const;
  Local<v8::Message> Message() const;
  MaybeLocal<Value> StackTrace(Local<Context>) const;

 private:
  Isolate* isolate_;
  /* Qualified because the member function `Message()` hides the class name. */
  mutable v8::Message* message_ = nullptr;
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
                       uint64_t this_value, const uint64_t* args, int length,
                       uint64_t data = 0)
      : isolate_(isolate),
        call_(call),
        this_value_(this_value),
        args_(args),
        length_(length),
        data_(data),
        return_value_(slag_undefined()) {}

  Isolate* GetIsolate() const { return isolate_; }
  int Length() const { return length_; }
  Local<T> operator[](int index) const { return Local<T>::New(args_[index]); }
  Local<T> This() const { return Local<T>::New(this_value_); }
  /* The value the template gave the callback (`FunctionCallbackInfo::Data`). */
  Local<Value> Data() const { return Local<Value>::New(data_); }
  ReturnValue<T> GetReturnValue() const {
    return ReturnValue<T>(&return_value_);
  }
  const void* call() const { return call_; }
  uint64_t return_value() const { return return_value_; }

 private:
  Isolate* isolate_;
  const void* call_;
  uint64_t this_value_;
  const uint64_t* args_;
  int length_;
  uint64_t data_;
  mutable uint64_t return_value_;
};

using FunctionCallback = void (*)(const FunctionCallbackInfo<Value>&);

/* What a property-handler callback receives (`v8::PropertyCallbackInfo`): the
 * object the internal method ran on, the configuration's data, and the slot its
 * answer goes in. */
template <class T>
class PropertyCallbackInfo {
 public:
  PropertyCallbackInfo(Isolate* isolate, uint64_t holder, uint64_t data,
                       uint64_t* return_value, const void* token)
      : isolate_(isolate),
        holder_(holder),
        data_(data),
        return_value_(return_value),
        token_(token) {}

  Isolate* GetIsolate() const { return isolate_; }
  Local<Object> This() const { return Local<Object>::New(holder_); }
  Local<Object> Holder() const { return Local<Object>::New(holder_); }
  Local<Value> Data() const { return Local<Value>::New(data_); }
  ReturnValue<T> GetReturnValue() const { return ReturnValue<T>(return_value_); }

  /* The running callback, as an opaque token. The facade uses it to route
   * allocation (`Object::New`, `Array::New`) and `Object::Set` through the
   * operation that called the handler; a host does not need to name it. */
  const void* token() const { return token_; }

 private:
  Isolate* isolate_;
  uint64_t holder_;
  uint64_t data_;
  uint64_t* return_value_;
  const void* token_;
};

/* The handler callbacks (`v8::NamedProperty*Callback`). The enumerator answers
 * with the keys the host owns, so it builds an array; a callback's allocations
 * go through the running window (`PropertyCallbackInfo::token`). */
using NamedPropertyGetterCallback =
    Intercepted (*)(Local<Name> property, const PropertyCallbackInfo<Value>& info);
using NamedPropertySetterCallback =
    Intercepted (*)(Local<Name> property, Local<Value> value,
                    const PropertyCallbackInfo<void>& info);
using NamedPropertyQueryCallback = Intercepted (*)(
    Local<Name> property, const PropertyCallbackInfo<Integer>& info);
using NamedPropertyDeleterCallback = Intercepted (*)(
    Local<Name> property, const PropertyCallbackInfo<Boolean>& info);
using NamedPropertyEnumeratorCallback = Intercepted (*)(
    const PropertyCallbackInfo<Array>& info);
using NamedPropertyDescriptorCallback = Intercepted (*)(
    Local<Name> property, const PropertyCallbackInfo<Value>& info);
using NamedPropertyDefinerCallback = Intercepted (*)(
    Local<Name> property, const PropertyDescriptor& descriptor,
    const PropertyCallbackInfo<Boolean>& info);

/* The indexed twins (`v8::IndexedProperty*Callback`): the key is an array
 * index, not a name. */
using IndexedPropertyGetterCallback = Intercepted (*)(
    uint32_t index, const PropertyCallbackInfo<Value>& info);
using IndexedPropertySetterCallback = Intercepted (*)(
    uint32_t index, Local<Value> value, const PropertyCallbackInfo<void>& info);
using IndexedPropertyQueryCallback = Intercepted (*)(
    uint32_t index, const PropertyCallbackInfo<Integer>& info);
using IndexedPropertyDeleterCallback = Intercepted (*)(
    uint32_t index, const PropertyCallbackInfo<Boolean>& info);
using IndexedPropertyEnumeratorCallback = Intercepted (*)(
    const PropertyCallbackInfo<Array>& info);
using IndexedPropertyDescriptorCallback = Intercepted (*)(
    uint32_t index, const PropertyCallbackInfo<Value>& info);
using IndexedPropertyDefinerCallback = Intercepted (*)(
    uint32_t index, const PropertyDescriptor& descriptor,
    const PropertyCallbackInfo<Boolean>& info);

/* How a handler is consulted (`v8::PropertyHandlerFlags`). Accepted and not
 * honoured: the engine's seam consults the host before its own lookup, so there
 * is no non-masking mode and no string-only mode to select. */
enum class PropertyHandlerFlags : int {
  kNone = 0,
  kAllCanRead = 1 << 0,
  kNonMasking = 1 << 1,
  kOnlyInterceptStrings = 1 << 2,
};

/* What a host installs on an object template
 * (`v8::NamedPropertyHandlerConfiguration`): all seven of V8's callbacks. The
 * parameter order follows V8's, with `data` and `flags` last. */
struct NamedPropertyHandlerConfiguration {
  NamedPropertyHandlerConfiguration(
      NamedPropertyGetterCallback getter = nullptr,
      NamedPropertySetterCallback setter = nullptr,
      NamedPropertyQueryCallback query = nullptr,
      NamedPropertyDeleterCallback deleter = nullptr,
      NamedPropertyEnumeratorCallback enumerator = nullptr,
      NamedPropertyDescriptorCallback descriptor = nullptr,
      NamedPropertyDefinerCallback definer = nullptr,
      Local<Value> data = Local<Value>(),
      PropertyHandlerFlags flags = PropertyHandlerFlags::kNone)
      : getter(getter),
        setter(setter),
        query(query),
        deleter(deleter),
        enumerator(enumerator),
        descriptor(descriptor),
        definer(definer),
        data(data),
        flags(flags) {}

  NamedPropertyGetterCallback getter;
  NamedPropertySetterCallback setter;
  NamedPropertyQueryCallback query;
  NamedPropertyDeleterCallback deleter;
  NamedPropertyEnumeratorCallback enumerator;
  NamedPropertyDescriptorCallback descriptor;
  NamedPropertyDefinerCallback definer;
  Local<Value> data;
  PropertyHandlerFlags flags;
};

/* What a host installs for element keys
 * (`v8::IndexedPropertyHandlerConfiguration`). A key that is an array index
 * goes to these when they are installed, and to the named handler otherwise. */
struct IndexedPropertyHandlerConfiguration {
  IndexedPropertyHandlerConfiguration(
      IndexedPropertyGetterCallback getter = nullptr,
      IndexedPropertySetterCallback setter = nullptr,
      IndexedPropertyQueryCallback query = nullptr,
      IndexedPropertyDeleterCallback deleter = nullptr,
      IndexedPropertyEnumeratorCallback enumerator = nullptr,
      IndexedPropertyDescriptorCallback descriptor = nullptr,
      IndexedPropertyDefinerCallback definer = nullptr,
      Local<Value> data = Local<Value>(),
      PropertyHandlerFlags flags = PropertyHandlerFlags::kNone)
      : getter(getter),
        setter(setter),
        query(query),
        deleter(deleter),
        enumerator(enumerator),
        descriptor(descriptor),
        definer(definer),
        data(data),
        flags(flags) {}

  IndexedPropertyGetterCallback getter;
  IndexedPropertySetterCallback setter;
  IndexedPropertyQueryCallback query;
  IndexedPropertyDeleterCallback deleter;
  IndexedPropertyEnumeratorCallback enumerator;
  IndexedPropertyDescriptorCallback descriptor;
  IndexedPropertyDefinerCallback definer;
  Local<Value> data;
  PropertyHandlerFlags flags;
};

/* How a constructor behaves when called without `new`
 * (`v8::ConstructorBehavior`). The facade accepts both and calls the handler
 * either way. */
enum class ConstructorBehavior { kThrow = 0, kAllow = 1 };

/* Whether a callback may have side effects (`v8::SideEffectType`). Accepted and
 * not consulted: the engine has no side-effect analysis to feed. */
enum class SideEffectType {
  kHasSideEffect = 0,
  kHasNoSideEffect = 1,
  kHasSideEffectToReceiver = 2,
};

/* A function value (`v8::Function`). */
class Function : public Object {
 public:
  static Local<Function> New(
      Local<Context> context, FunctionCallback callback,
      Local<Value> data = Local<Value>(), int length = 0,
      ConstructorBehavior behavior = ConstructorBehavior::kAllow,
      SideEffectType side_effect_type = SideEffectType::kHasSideEffect);

  MaybeLocal<Value> Call(Local<Context> context, Local<Value> recv, int argc,
                         Local<Value> argv[]);
  MaybeLocal<Object> NewInstance(Local<Context> context, int argc,
                                 Local<Value> argv[]);
};

/* A function template's signature (`v8::Signature` and `v8::AccessorSignature`):
 * the type a call or access is declared over. `New` records the template, and
 * the check is the receiver's prototype chain: a method installed with a
 * signature only runs on an instance of that template's built prototype, which
 * is what V8's access check answers. */
class Signature {
 public:
  static Local<Signature> New(Isolate* isolate,
                              Local<FunctionTemplate> function);

  Local<FunctionTemplate> GetFunctionTemplate() const;

 private:
  friend class FunctionTemplate;
  explicit Signature(FunctionTemplate* function) : function_(function) {}
  FunctionTemplate* function_ = nullptr;
};

class AccessorSignature {
 public:
  static Local<AccessorSignature> New(Isolate* isolate,
                                      Local<FunctionTemplate> function);
  Local<FunctionTemplate> GetFunctionTemplate() const;

 private:
  friend class Object;
  friend class ObjectTemplate;
  explicit AccessorSignature(FunctionTemplate* function) : function_(function) {}
  /* The template the check walks to. The trampolines read it from the accessor
   * specification rather than from a handle. */
  FunctionTemplate* function() const { return function_; }
  FunctionTemplate* function_ = nullptr;
};

class FunctionTemplate {
 public:
  static Local<FunctionTemplate> New(
      Isolate* isolate, FunctionCallback callback,
      Local<Value> data = Local<Value>(),
      Local<Signature> signature = Local<Signature>(), int length = 0,
      ConstructorBehavior behavior = ConstructorBehavior::kAllow,
      SideEffectType side_effect_type = SideEffectType::kHasSideEffect);

  FunctionCallback callback() const { return callback_; }
  /* The built prototype instances inherit from, 0 until the template is
   * materialized. It is the identity a signature's receiver check walks to
   * (`AllowsReceiver`). */
  uint64_t prototype_bits() const { return prototype_bits_; }

  /* The type the template's `Signature` declares, or null. The receiver check
   * takes this, so a trampoline never has to unwrap a `Signature` itself. */
  FunctionTemplate* signature_type() const {
    return signature_ == nullptr ? nullptr : signature_->function_;
  }
  /* The value the template hands its callbacks (`FunctionCallbackInfo::Data`). */
  Local<Value> data() const { return Local<Value>::New(data_bits_); }
  void SetCallHandler(FunctionCallback callback,
                      Local<Value> data = Local<Value>());
  void SetClassName(const char* name);
  void Inherit(Local<FunctionTemplate> parent);

  Local<ObjectTemplate> InstanceTemplate();
  Local<ObjectTemplate> PrototypeTemplate();

  /* Materialize the function (`FunctionTemplate::GetFunction`): an instance
   * inherits from `InstanceTemplate`'s object (which also carries the
   * `PrototypeTemplate`'s entries), with the template's internal fields and
   * API-wrapper bit. */
  MaybeLocal<Function> GetFunction(Local<Context> context);

  /* Whether this template's signature admits `receiver` as `this`: its built
   * prototype must be on the receiver's chain. A template without a signature
   * admits any receiver. Called by the call trampolines. */
  bool AllowsReceiver(uint64_t receiver) const;

 private:
  friend class Signature;
  friend class AccessorSignature;
  explicit FunctionTemplate(FunctionCallback callback) : callback_(callback) {}

  /* The object `new` instances inherit from: the instance template's object,
   * chained to the parent's and carrying the prototype template's members. Built
   * once and kept, so every instance of one template shares a prototype (which is
   * what makes a signature's check an identity test). */
  MaybeLocal<Object> BuildPrototypeObject(Local<Context> context);

  FunctionCallback callback_;
  uint64_t data_bits_ = 0;
  std::string class_name_;
  int length_ = 0;
  bool allow_construction_ = true;
  FunctionTemplate* parent_ = nullptr;
  ObjectTemplate* instance_template_ = nullptr;
  ObjectTemplate* prototype_template_ = nullptr;
  /* The declared signature, and the built prototype the check compares against
   * (rooted, and 0 until the template is materialized). */
  Signature* signature_ = nullptr;
  uint64_t prototype_bits_ = 0;
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

  /* How many internal fields each instance carries
   * (`v8::ObjectTemplate::SetInternalFieldCount`). */
  void SetInternalFieldCount(int count) { internal_fields_ = count; }
  int InternalFieldCount() const { return internal_fields_; }

  /* Install a named-property handler (`v8::ObjectTemplate::SetHandler`): every
   * instance's property access is offered to the handler first. */
  void SetHandler(const NamedPropertyHandlerConfiguration& configuration);

  /* The same for element keys
   * (`v8::ObjectTemplate::SetHandler` with an indexed configuration). */
  void SetHandler(const IndexedPropertyHandlerConfiguration& configuration);

  /* Install an accessor on every instance (`v8::ObjectTemplate::SetAccessor`):
   * a read of `name` runs `getter` and a write runs `setter`, with `data`
   * arriving on the callback info. */
  void SetAccessor(Local<Name> name, AccessorNameGetterCallback getter,
                   AccessorNameSetterCallback setter = nullptr,
                   Local<Value> data = Local<Value>(),
                   PropertyAttribute attribute = None);
  /* The same, declared over a receiver type (`v8::AccessorSignature`): the
   * getter and setter only run when `this` is an instance of that template's
   * built prototype, and fail with `TypeError: Illegal invocation` otherwise.
   * `signature` has no default, so an ordinary call cannot land on this
   * overload. */
  void SetAccessor(Local<Name> name, AccessorNameGetterCallback getter,
                   AccessorNameSetterCallback setter, Local<Value> data,
                   Local<AccessorSignature> signature,
                   PropertyAttribute attribute = None);

  /* The data-property forms (`v8::ObjectTemplate::SetNativeDataProperty` and
   * `SetLazyDataProperty`): both install the same accessor. The engine has no
   * one-shot slot, so a lazy getter runs on every read rather than once, and
   * the property is an accessor rather than a data slot. */
  void SetNativeDataProperty(Local<Name> name, AccessorNameGetterCallback getter,
                             AccessorNameSetterCallback setter = nullptr,
                             Local<Value> data = Local<Value>(),
                             PropertyAttribute attribute = None);
  void SetLazyDataProperty(Local<Name> name, AccessorNameGetterCallback getter,
                           Local<Value> data = Local<Value>(),
                           PropertyAttribute attribute = None);

  /* Install a call-as-function handler
   * (`v8::ObjectTemplate::SetCallAsFunctionHandler`). An instance built from a
   * template with one is callable: the facade makes a builtin whose body is the
   * handler, with the template's members and internal fields on the callee
   * itself. A template that also has a property handler keeps the handler
   * object instead, because the engine's host objects have no [[Call]]. */
  void SetCallAsFunctionHandler(FunctionCallback callback,
                                Local<Value> data = Local<Value>()) {
    call_as_function_ = callback;
    call_as_function_data_ = data;
  }
  FunctionCallback call_as_function() const { return call_as_function_; }
  Local<Value> call_as_function_data() const { return call_as_function_data_; }

  /* Install a per-object access check (`ObjectTemplate::SetAccessCheckCallback`
   * and `SetAccessCheckCallbackAndHandler`): a property access to an instance
   * from another realm consults `callback`, which answers whether it may
   * proceed. An instance is an engine host object, so no compiled fast path can
   * serve the property before the check runs. The check fires only on a
   * cross-realm access, which is where V8 consults it too; a call the callback
   * refuses raises `TypeError: Illegal access`. */
  using AccessCheckCallback = bool (*)(Local<Context> accessing_context,
                                       Local<Object> accessed_object,
                                       Local<Value> data);
  void SetAccessCheckCallback(Isolate* isolate, AccessCheckCallback callback,
                              Local<Value> data = Local<Value>());
  /* The same, and a named-property handler: this is the pair Blink configures
   * on a cross-origin wrapper (`SetAccessCheckCallbackAndHandler`). The check
   * has no data of its own here, so the configuration's `data` is handed to
   * both. */
  void SetAccessCheckCallbackAndHandler(
      Isolate* isolate, AccessCheckCallback callback,
      const NamedPropertyHandlerConfiguration& configuration);
  AccessCheckCallback access_check() const { return access_check_; }
  Local<Value> access_check_data() const {
    return Local<Value>::New(access_check_data_bits_);
  }

  /* One installed accessor: the name and data are retained for the template's
   * lifetime (templates are not freed), and their bits are kept so the
   * trampolines do not have to make a handle per access. `Global` is defined
   * later in this header, so the retention is the ABI's root registry. */
  struct Accessor {
    uint64_t name_bits = 0;
    uint64_t data_bits = 0;
    AccessorNameGetterCallback getter = nullptr;
    AccessorNameSetterCallback setter = nullptr;
    /* The receiver type the accessor is declared over (`AccessorSignature`).
     * The getter and setter trampolines run the same `AllowsReceiver` check a
     * function template's signature does, so an accessor and a method cannot
     * disagree about what an instance is. */
    FunctionTemplate* signature = nullptr;
    int32_t attributes = 0;
  };
  const std::deque<Accessor>& accessors() const { return accessors_; }

  /* Create an instance of this template: an ordinary object with the template's
   * internal fields, its host functions, and `IsApiWrapper` set. A template with
   * a handler makes an object whose property access goes to it, and a template
   * with an access check makes one that refuses a cross-realm access. */
  MaybeLocal<Object> NewInstance(Local<Context> context);

 private:
  std::vector<Entry> entries_;
  std::deque<Accessor> accessors_;
  int internal_fields_ = 0;
  FunctionCallback call_as_function_ = nullptr;
  Local<Value> call_as_function_data_;
  NamedPropertyHandlerConfiguration named_handler_;
  IndexedPropertyHandlerConfiguration indexed_handler_;
  bool has_named_handler_ = false;
  bool has_indexed_handler_ = false;
  AccessCheckCallback access_check_ = nullptr;
  /* Whether a check was installed at all: a null callback still asks for one,
   * and the default check is the security-token comparison. */
  bool has_access_check_ = false;
  /* The check's `data`, retained for the template's lifetime (templates are not
   * freed), like an accessor's name and data. */
  uint64_t access_check_data_bits_ = 0;
};

class Context {
 public:
  static Local<Context> New(
      Isolate* isolate, std::nullptr_t = nullptr,
      Local<ObjectTemplate> global_template = Local<ObjectTemplate>());
  /* With a host microtask queue (`v8::ContextOptions::microtask_queue`): that
   * realm's promise jobs go to `microtask_queue`, so the host drains them. A
   * null queue is the default, which `GetMicrotaskQueue` creates. */
  static Local<Context> New(Isolate* isolate, std::nullptr_t,
                            Local<ObjectTemplate> global_template,
                            MicrotaskQueue* microtask_queue);
  ~Context();

  /* This context's microtask queue (`v8::Context::GetMicrotaskQueue`). Never
   * null: a context made without one made its own. */
  MicrotaskQueue* GetMicrotaskQueue() const { return microtask_queue_; }

  Local<Object> Global() const;

  /* The isolate this context was made on (`v8::Context::GetIsolate`). */
  Isolate* GetIsolate() const { return isolate_; }

  /* The A0 C ABI context behind this facade object. */
  SlagContext* slag() const { return slag_; }

  /* The host's embedder data (`Context::SetEmbedderData`,
   * `SetAlignedPointerInEmbedderData`, and their readers). A value slot roots
   * what it holds; a pointer slot is the host's address and is not traced. The
   * two stores are separate here: the engine has no tagged slot array, and the
   * Rust bridge keeps only the pointer form. */
  void SetAlignedPointerInEmbedderData(int index, void* value);
  void* GetAlignedPointerFromEmbedderData(int index);
  void SetEmbedderData(int index, Local<Value> value);
  Local<Value> GetEmbedderData(int index);
  uint32_t GetNumberOfEmbedderDataFields();
  void ClearAllSlots();

  /* The token another context must answer to be allowed to reach this one's
   * properties (`Context::GetSecurityToken`). Stored and not consulted: the
   * engine's per-object access check compares realms, not tokens, so a token
   * gates nothing yet. A cross-origin wrapper is protected by its template's
   * access check (`ObjectTemplate::SetAccessCheckCallback`). */
  Local<Value> GetSecurityToken();
  void SetSecurityToken(Local<Value> token);
  void UseDefaultSecurityToken();

  /* The debugger surface. These are not `v8.h` methods: V8 puts the same
   * machinery behind the inspector protocol, which is A5's cost, so the facade
   * exposes the engine's own protocol instead (see the README's Debugger
   * section).
   *
   * `SetDebugAttached` opens a session. The engine emits a debug point per
   * statement and per loop back edge only while one is attached, and recompiles
   * its bytecode on both transitions, so a normal run carries no points at all.
   * `SetBreakpoints` takes source offsets into the script text, which is what
   * resolving a (line, column) with `v8::Message` gives. `RequestPause` stops at
   * the next point the run reaches. `Paused` reports a stopped run and where it
   * stopped, and `Resume` continues it, answering the value the script
   * completes with.
   *
   * A stop parks every activation it interrupted, so a breakpoint inside a
   * function stops there; `Resume` continues them, outermost last. The same
   * stops are what `v8-inspector.h` reports as `Debugger.paused`. */
  struct PauseInfo {
    enum Reason { kNone = 0, kRequested = 1, kBreakpoint = 2, kStep = 3 };
    Reason reason = kNone;
    /* The debug point's span: byte offsets into the script text. */
    uint32_t start = 0;
    uint32_t end = 0;
  };
  void SetDebugAttached(bool attached);
  void SetBreakpoints(const uint32_t* offsets, size_t count);
  void RequestPause();
  /* Ask the next Resume to stop after one step: StepOver at the next point of
   * the halted activation or an outer one, StepInto at the next point anywhere,
   * StepOut once the halted activation has returned to a caller. */
  void StepInto();
  void StepOver();
  void StepOut();
  bool Paused(PauseInfo* out) const;
  MaybeLocal<Value> Resume();
  /* Evaluate `source` at the stopped position (`Debugger.evaluateOnCallFrame`).
   * The evaluation is compiled against the halted body's scope, so a certified
   * body's slot locals are both readable and writable: an assignment reaches the
   * local the resumed body will read. */
  MaybeLocal<Value> EvaluateOnPausedFrame(Local<String> source);

  class Scope {
   public:
    explicit Scope(Local<Context> context)
        : previous_(g_current_context), realm_(0) {
      g_current_context = context.raw();
      // The engine's current realm follows the entered context, so a host-side
      // access compares against the context it was made in rather than
      // whichever realm was created last. An empty handle enters no realm, and
      // the ABI answers the one already current, so the exit pair stays
      // balanced either way.
      SlagContext* slag = nullptr;
      if (context.raw() != 0) {
        slag = reinterpret_cast<Context*>(
                   static_cast<uintptr_t>(context.raw()))
                   ->slag();
      }
      realm_ = slag_context_enter(slag);
    }
    ~Scope() {
      g_current_context = previous_;
      slag_context_leave(realm_);
    }

   private:
    uint64_t previous_;
    uint32_t realm_;
  };

 private:
  SlagContext* slag_ = nullptr;
  Isolate* isolate_ = nullptr;
  std::vector<void*> pointer_slots_;
  std::vector<uint64_t> value_slots_;
  /* The queue this context's microtasks go to, and whether this context owns
   * it: a queue the host passed in is the host's to keep alive. */
  MicrotaskQueue* microtask_queue_ = nullptr;
  bool owns_microtask_queue_ = false;
  /* Retained while set; zero means "the isolate's default". */
  uint64_t security_token_ = 0;

  /* The token this context is compared by: its own, or the isolate's shared
   * default when it set none. Two contexts that never set one answer the same
   * value whether or not the default has been minted yet. */
  uint64_t effective_security_token() const;

  friend class Isolate;
};

/* Thread locking (`v8::Locker`/`v8::Unlocker`). The engine is single-threaded
 * per isolate, so these record nothing and never block; a host that needs
 * cross-thread sharing needs the engine to grow a locker, not this facade. */
class Locker {
 public:
  explicit Locker(Isolate*) {}
  ~Locker() = default;
  Locker(const Locker&) = delete;
  Locker& operator=(const Locker&) = delete;
};

class Unlocker {
 public:
  explicit Unlocker(Isolate*) {}
  ~Unlocker() = default;
};

/* A scope that drains the microtask queue as it exits (`v8::MicrotasksScope`):
 * the outermost `kRunMicrotasks` scope drains through the isolate's checkpoint.
 * `kDoNotRunMicrotasks` leaves the queue alone. */
class MicrotasksScope {
 public:
  enum Type { kDoNotRunMicrotasks, kRunMicrotasks };

  MicrotasksScope(Isolate* isolate, Type type);
  ~MicrotasksScope();

  MicrotasksScope(const MicrotasksScope&) = delete;
  MicrotasksScope& operator=(const MicrotasksScope&) = delete;

  static int GetCurrentDepth(Isolate* isolate);
  static bool IsRunningMicrotasks(Isolate* isolate);

 private:
  Isolate* isolate_;
  Type type_;
};

/* The kind of collection a GC callback was told about (`v8::GCType`). */
enum GCType {
  kGCTypeScavenge = 1 << 0,
  kGCTypeMarkSweepCompact = 1 << 1,
  kGCTypeAll = kGCTypeScavenge | kGCTypeMarkSweepCompact,
};

/* The flags a GC callback receives (`v8::GCCallbackFlags`). Accepted and passed
 * through; the facade has no retained-object infos to construct. */
enum GCCallbackFlags {
  kNoGCCallbackFlags = 0,
  kGCCallbackFlagConstructRetainedObjectInfos = 1 << 1,
  kGCCallbackFlagForced = 1 << 2,
  kGCCallbackFlagSynchronousPhantomCallbackProcessing = 1 << 3,
};

/* Which collection to request (`Isolate::RequestGarbageCollectionForTesting`).
 * The engine has one full collection, so both names run it. */
enum GarbageCollectionType {
  kFullGarbageCollection = 0,
  kMinorGarbageCollection = 1,
};

/* The heap numbers `Isolate::GetHeapStatistics` fills (`v8::HeapStatistics`).
 * The four measured fields come from the arena walk and the buffer record; the
 * constants are the ones the Rust bridge's own `HeapStatistics` documents. */
struct HeapStatistics {
  size_t total_heap_size = 0;
  size_t total_heap_size_executable = 0;
  size_t total_physical_size = 0;
  size_t total_available_size = 0;
  size_t used_heap_size = 0;
  size_t heap_size_limit = 0;
  size_t malloced_memory = 0;
  size_t external_memory = 0;
  size_t peak_malloced_memory = 0;
  bool does_zap_garbage = false;
  size_t number_of_native_contexts = 0;
  size_t number_of_detached_contexts = 0;
  size_t total_global_handles_size = 0;
  size_t used_global_handles_size = 0;
  size_t embedded_blob_size = 0;
};

/* A startup snapshot (`v8::StartupData`): the bytes `SnapshotCreator::CreateBlob`
 * produced, which the caller frees with `delete[]`. The rehash flag is always
 * true and validity is "has bytes", because the engine's blob has no weaker
 * form than that. */
struct StartupData {
  const char* data = nullptr;
  int raw_size = 0;

  bool can_be_rehashed() const { return true; }
  bool is_valid() const { return data != nullptr && raw_size > 0; }
};

class Isolate {
 public:
  struct CreateParams {
    ArrayBuffer::Allocator* array_buffer_allocator = nullptr;
    /* A blob from `SnapshotCreator::CreateBlob`: the first context this isolate
     * creates has it restored onto its global (`v8::CreateParams`). */
    const StartupData* snapshot_blob = nullptr;
  };

  static Isolate* New(const CreateParams& params);
  void Dispose();
  /* The isolate entered on this thread (`Isolate::GetCurrent`), or null. The
   * facade's `GetCurrent` and `TryGetCurrent` are the same answer. */
  static Isolate* GetCurrent();
  static Isolate* TryGetCurrent();
  void ThrowError(const char* message);
  /* Throw `exception` (`Isolate::ThrowException`): the pending exception
   * becomes exactly that language value. */
  void ThrowException(Local<Value> exception);

  /* How `Isolate::AddMessageListenerWithErrorLevel` filters. */
  enum MessageErrorLevel {
    kMessageLog = (1 << 0),
    kMessageDebug = (1 << 1),
    kMessageInfo = (1 << 2),
    kMessageWarning = (1 << 3),
    kMessageError = (1 << 4),
    kAllMessageLevels = 0x1f,
    kNoMessageLevels = 0
  };
  /* `(message, data)`: answer false to let the next listener see it. This is
   * V8's `MessageCallback`, and it is how a host (the inspector included) hears
   * about an error nothing caught. */
  using MessageCallback = bool (*)(Local<Message> message, Local<Value> data);
  void AddMessageListener(MessageCallback callback,
                          Local<Value> data = Local<Value>());
  void AddMessageListenerWithErrorLevel(MessageCallback callback,
                                        int message_levels,
                                        Local<Value> data = Local<Value>());
  bool RemoveMessageListener(MessageCallback callback);
  bool RemoveMessageListenerWithErrorLevel(MessageCallback callback,
                                           int message_levels);
  Local<Context> GetCurrentContext();
  /* The entered context (`Isolate::GetEnteredContext` and
   * `Isolate::GetEnteredOrMicrotaskContext`). The facade enters a context only
   * through `Context::Scope`, so both answer the same handle as
   * `GetCurrentContext`; there is no microtask-context distinction to make. */
  Local<Context> GetEnteredContext();
  Local<Context> GetEnteredOrMicrotaskContext();

  /* Whether `accessing_context` may reach `accessed_object`: an object of the
   * accessing realm always, and anything else only when the two contexts answer
   * the same security token (`v8::Isolate::MayAccess`). A realm no context of
   * this isolate claims is treated as the caller's own. */
  bool MayAccess(Local<Context> accessing_context, Local<Object> accessed_object);

  /* Drain the job queues now (`Isolate::PerformMicrotaskCheckpoint`). */
  void PerformMicrotaskCheckpoint();

  /* Add `change` bytes to the host's external-memory account and answer the new
   * total, the engine's live buffer bytes included
   * (`Isolate::AdjustAmountOfExternalAllocatedMemory`). */
  int64_t AdjustAmountOfExternalAllocatedMemory(int64_t change);

  /* The heap's numbers (`Isolate::GetHeapStatistics`). The arena is shared by
   * the isolate's contexts, so the first entered one answers it; each further
   * context's live buffers are added to the external total. */
  void GetHeapStatistics(HeapStatistics* statistics);

  /* Collect now (`Isolate::RequestGarbageCollectionForTesting` and
   * `LowMemoryNotification`), running the prologue and epilogue callbacks
   * around it. The engine also collects on its own growth policy, and those
   * collections do not reach these callbacks: the hook is the facade's around
   * an explicit request. */
  void RequestGarbageCollectionForTesting(GarbageCollectionType type);
  void LowMemoryNotification();

  using GCCallback = void (*)(Isolate* isolate, GCType type,
                              GCCallbackFlags flags, void* data);
  void AddGCEpilogueCallback(GCCallback callback, void* data = nullptr);
  void RemoveGCEpilogueCallback(GCCallback callback, void* data = nullptr);
  void AddGCPrologueCallback(GCCallback callback, void* data = nullptr);
  void RemoveGCPrologueCallback(GCCallback callback, void* data = nullptr);

  /* The trampoline the engine's collection observer calls: `prologue` selects
   * the list, and `type` is the generation it was told about. */
  void NotifyGcCallbacks(GCType type, bool prologue);

  /* When the job queues drain (`Isolate::SetMicrotasksPolicy`). `kAuto` is the
   * default: a successful `Script::Run` drains when it returns. */
  void SetMicrotasksPolicy(MicrotasksPolicy policy) {
    microtasks_policy_ = policy;
  }
  MicrotasksPolicy GetMicrotasksPolicy() const { return microtasks_policy_; }

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

  /* Run every context's queued jobs, answering whether any was queued
   * (`platform::PumpMessageLoop`). */
  bool PumpMessages();

  /* The context of this isolate whose realm is `realm`, or null when none is
   * (an object the host built outside every context). The realm is the engine's
   * fact (`slag_value_realm`); this is the facade's context for it. */
  Context* ContextForRealm(uint32_t realm);

  SlagIsolate* slag_ = nullptr;
  std::vector<Context*> contexts_;
  MicrotasksPolicy microtasks_policy_ = MicrotasksPolicy::kAuto;
  std::vector<std::pair<GCCallback, void*>> prologue_callbacks_;
  std::vector<std::pair<GCCallback, void*>> epilogue_callbacks_;
  /* `Private::ForApi`'s one-name-one-private registry: the description and the
   * symbol bits, retained until `Dispose`. */
  std::unordered_map<std::string, uint64_t> private_names_;
  /* The default security token, minted on the first ask and shared by every
   * context without one of its own. Retained until `Dispose`. */
  uint64_t default_security_token_ = 0;
  struct MessageListener {
    MessageCallback callback = nullptr;
    int levels = kAllMessageLevels;
    /* The `data` value, retained until `Dispose` (or the listener is removed). */
    uint64_t data_bits = 0;
  };
  std::vector<MessageListener> message_listeners_;

  /* Deliver the pending error to the listeners that asked for `level`. A
   * `TryCatch` suppresses the report, so the caller checks the depth first. */
  static void NotifyMessageListeners(Isolate* isolate, SlagContext* slag,
                                     int level);

  friend class Script;
  /* `CreateParams::array_buffer_allocator`: the host's allocator for the backing
   * stores this isolate makes. Null means the engine allocates its own. */
  ArrayBuffer::Allocator* array_buffer_allocator_ = nullptr;
  /* The startup blob `CreateParams::snapshot_blob` named, and whether the first
   * context has consumed it. */
  std::vector<uint8_t> snapshot_blob_;
  bool snapshot_consumed_ = false;

  friend class Context;
  friend class Private;
  friend class ArrayBuffer;
  friend class Object;
  friend bool platform::PumpMessageLoop(Platform*, Isolate*);
};

/* The host's external-memory account scoped to an object's lifetime
 * (`v8::ExternalMemoryAccounter`): `Update` keeps one number, and the
 * destructor gives back whatever is still recorded. The account belongs to one
 * isolate; switching isolates is not supported, as in V8. */
class ExternalMemoryAccounter {
 public:
  ExternalMemoryAccounter() = default;
  explicit ExternalMemoryAccounter(size_t size) : size_(size) {}
  ~ExternalMemoryAccounter();

  ExternalMemoryAccounter(const ExternalMemoryAccounter&) = delete;
  ExternalMemoryAccounter& operator=(const ExternalMemoryAccounter&) = delete;

  void Update(Isolate* isolate, size_t size);
  void Increase(Isolate* isolate, size_t size);
  void Decrease(Isolate* isolate, size_t size);

 private:
  Isolate* isolate_ = nullptr;
  size_t size_ = 0;
};

/* A host that writes a startup snapshot (`v8::SnapshotCreator`). It owns an
 * isolate, the host builds the default context in it, and `CreateBlob`
 * serializes the realm's global bindings. Disposing the creator disposes the
 * isolate. */
class SnapshotCreator {
 public:
  explicit SnapshotCreator(const intptr_t* external_references = nullptr);
  ~SnapshotCreator();

  SnapshotCreator(const SnapshotCreator&) = delete;
  SnapshotCreator& operator=(const SnapshotCreator&) = delete;

  Isolate* GetIsolate() const { return isolate_; }
  void SetDefaultContext(Local<Context> context);

  /* The blob for the default context, which the caller frees with `delete[]`. */
  StartupData CreateBlob();

 private:
  Isolate* isolate_ = nullptr;
  uint64_t default_context_ = 0;
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

  static HandleSlot* CreateHandle(uint64_t value, bool rooted);

 private:
  std::size_t mark_;
};

template <class T>
Local<T> Local<T>::New(uint64_t value) {
  return Local<T>(
      HandleScope::CreateHandle(value, std::is_base_of_v<Value, T>));
}

/* How a weak handle is watched (`v8::WeakCallbackType`). All three run the same
 * parameter-based callback here: the engine reports the death, not which kind
 * of reference it was holding. */
enum class WeakCallbackType {
  kParameter = 0,
  kInternalFields = 1,
  kFinalizer = 2,
};

/* The context a weak death callback is handed (`v8::WeakCallbackInfo`). `T` is
 * the type of the pointer `SetWeak` was given, as in V8. The value is already
 * gone, so `GetValue` answers empty; `GetParameter` is the host's own pointer,
 * which is what a finalizer uses. */
template <class T>
class WeakCallbackInfo {
 public:
  using Callback = void (*)(const WeakCallbackInfo<T>&);

  WeakCallbackInfo(Isolate* isolate, void* parameter)
      : isolate_(isolate), parameter_(parameter) {}

  Isolate* GetIsolate() const { return isolate_; }
  T* GetParameter() const { return static_cast<T*>(parameter_); }
  Local<Value> GetValue() const { return Local<Value>(); }

  /* A guaranteed finalizer for the same death: it runs after every first pass
   * of the drain, even if this pass released the handle. */
  void SetSecondPassCallback(Callback callback) const;

 private:
  Isolate* isolate_;
  void* parameter_;
};

namespace internal {

template <class T>
struct WeakCallbackSpec {
  Isolate* isolate;
  void* parameter;
  typename WeakCallbackInfo<T>::Callback callback;
};

template <class T>
struct WeakSecondPassSpec {
  Isolate* isolate;
  void* parameter;
  typename WeakCallbackInfo<T>::Callback callback;
};

}  // namespace internal

template <class T>
inline void WeakCallbackTrampoline(void* data) {
  auto* spec = static_cast<internal::WeakCallbackSpec<T>*>(data);
  WeakCallbackInfo<T> info(spec->isolate, spec->parameter);
  spec->callback(info);
  delete spec;
}

template <class T>
inline void WeakSecondPassTrampoline(void* data) {
  auto* spec = static_cast<internal::WeakSecondPassSpec<T>*>(data);
  WeakCallbackInfo<T> info(spec->isolate, spec->parameter);
  spec->callback(info);
  delete spec;
}

template <class T>
inline void WeakCallbackInfo<T>::SetSecondPassCallback(Callback callback) const {
  auto* spec =
      new internal::WeakSecondPassSpec<T>{isolate_, parameter_, callback};
  slag_weak_queue_second_pass(spec, &WeakSecondPassTrampoline<T>);
}

/* A persistent handle. It outlives every `HandleScope`, so the C ABI's root
 * registry has to hold what it names for exactly as long as this object exists;
 * a handle slot cannot do that, because the scope that allocated it pops.
 *
 * A host must reset or destroy a `Global` before disposing its isolate, the
 * rule V8 states for persistents: the registry is thread-local, not owned by
 * an isolate, so a leaked `Global` keeps naming a box the freed heap reused. */
template <class T>
class Global {
 public:
  Global() = default;

  // V8's persistent handles are not copyable. A `Global` owns one retention
  // and must release it exactly once, so a copy would either double-release or
  // silently alias the same retention.
  Global(const Global&) = delete;
  Global& operator=(const Global&) = delete;

  Global(Global&& other) noexcept { Adopt(other); }
  Global& operator=(Global&& other) noexcept {
    if (this != &other) {
      Release();
      Adopt(other);
    }
    return *this;
  }

  Global(Isolate*, Local<T> local) { Hold(local); }
  ~Global() { Release(); }

  bool IsEmpty() const { return empty_; }
  Local<T> Get(Isolate*) const {
    if (empty_) return Local<T>();
    if (weak_ != nullptr) {
      uint64_t live = 0;
      if (slag_weak_get(weak_, &live) != 0) return Local<T>();
      return Local<T>::New(live);
    }
    return Local<T>::New(value_);
  }

  void Reset() { Release(); }
  void Reset(Isolate*, Local<T> local) {
    Release();
    Hold(local);
  }

  /* Watch the value without keeping it alive (`Global::SetWeak`). The strong
   * retention is released here, and `callback` runs after the collection that
   * takes the value. A facade pointer is held strongly instead: there is nothing
   * for the engine to watch. */
  template <class P>
  void SetWeak(P* parameter, typename WeakCallbackInfo<P>::Callback callback,
               WeakCallbackType type) {
    (void)type;
    if (empty_ || !kEngineValue || weak_ != nullptr) return;
    weak_ = slag_weak_new(value_);
    slag_root_release(value_);
    // The word must go: a `Global` is usually a local, and the conservative
    // stack scan would read these bits as a root and keep the value alive, so
    // the weak handle would never empty. `Get` reads the live bits from the
    // engine instead.
    value_ = 0;
    // The engine owns the specification: it runs the trampoline once and the
    // trampoline frees it.
    auto* spec = new internal::WeakCallbackSpec<P>{
        g_current_isolate, static_cast<void*>(parameter), callback};
    slag_weak_set_callback(weak_, spec, &WeakCallbackTrampoline<P>);
  }

  /* Take the value back under a strong hold (`Global::ClearWeak`), while it is
   * still alive. A value the collector already took leaves the handle empty. */
  void ClearWeak() {
    if (weak_ == nullptr) return;
    uint64_t live = 0;
    if (slag_weak_get(weak_, &live) == 0) {
      slag_root_retain(live);
      value_ = live;
      empty_ = false;
    } else {
      empty_ = true;
    }
    slag_weak_free(weak_);
    weak_ = nullptr;
  }

  bool IsWeak() const { return weak_ != nullptr; }

  void SetWrapperClassId(uint16_t class_id) { wrapper_class_id_ = class_id; }
  uint16_t WrapperClassId() const { return wrapper_class_id_; }

 private:
  // A `T` that derives from `Value` is an engine value and the registry must
  // hold it. A facade pointer (`Context`, `Script`, template) is a C++
  // allocation, and tracing it as a `Value` would be unsound.
  static constexpr bool kEngineValue = std::is_base_of_v<Value, T>;

  void Hold(Local<T> local) {
    empty_ = local.IsEmpty();
    value_ = empty_ ? 0 : local.raw();
    if (!empty_ && kEngineValue) slag_root_retain(value_);
  }

  // Ownership transfers, so this retains nothing and releases nothing.
  void Adopt(Global& other) noexcept {
    value_ = other.value_;
    empty_ = other.empty_;
    weak_ = other.weak_;
    wrapper_class_id_ = other.wrapper_class_id_;
    other.value_ = 0;
    other.empty_ = true;
    other.weak_ = nullptr;
  }

  void Release() {
    if (weak_ != nullptr) {
      // Clears the engine entry and frees the box; a callback already run left
      // the entry gone, which `slag_weak_free` treats as a no-op.
      slag_weak_free(weak_);
      weak_ = nullptr;
    } else if (!empty_ && kEngineValue) {
      slag_root_release(value_);
    }
    value_ = 0;
    empty_ = true;
  }

  uint64_t value_ = 0;
  bool empty_ = true;
  SlagWeak* weak_ = nullptr;
  uint16_t wrapper_class_id_ = 0;
};

/* A host-traced root: the engine keeps the value alive for as long as this
 * object exists, and the host says when it is gone. V8's `TracedReference` is
 * meant for an object the embedder's own collector manages; this facade has no
 * embedder-heap tracing, so the reference is simply a root the C++ side owns
 * and a host must release (`Reset` or the destructor) like any persistent.
 *
 * A copy is a second reference, not another handle to the same one, so
 * resetting one does not clear the other. That is the engine's `Traced`
 * semantics, and what Blink's `TraceWrapperV8Reference` relies on. */
class TracedReferenceBase {
 public:
  TracedReferenceBase() = default;
  ~TracedReferenceBase() { Reset(); }

  TracedReferenceBase(const TracedReferenceBase& other)
      : traced_(CloneTraced(other.traced_)) {}
  TracedReferenceBase& operator=(const TracedReferenceBase& other) {
    if (this != &other) {
      Reset();
      traced_ = CloneTraced(other.traced_);
    }
    return *this;
  }

  bool IsEmpty() const { return traced_ == nullptr; }

  void Reset() {
    if (traced_ != nullptr) {
      slag_traced_free(traced_);
      traced_ = nullptr;
    }
  }

 protected:
  void Adopt(uint64_t value, bool empty) {
    Reset();
    if (!empty) traced_ = slag_traced_new(value);
  }

  /* The tagged word, `false` when the reference is empty or the engine has
   * released the value. The word is not tested against 0: `+0.0` is a value
   * whose bits are 0. */
  bool TryValue(uint64_t* out) const {
    return traced_ != nullptr && slag_traced_get(traced_, out) == 0;
  }

 private:
  static SlagTraced* CloneTraced(SlagTraced* traced) {
    return traced == nullptr ? nullptr : slag_traced_clone(traced);
  }

  SlagTraced* traced_ = nullptr;
};

template <class T>
class TracedReference : public TracedReferenceBase {
 public:
  // The templated `Reset` below would hide the base's release.
  using TracedReferenceBase::Reset;

  TracedReference() = default;
  TracedReference(Isolate*, Local<T> that) { Reset(nullptr, that); }

  template <class S, class = std::enable_if_t<std::is_base_of_v<T, S>>>
  TracedReference(const TracedReference<S>& other)
      : TracedReferenceBase(other) {}

  /* The value, while the host has not released this reference. */
  Local<T> Get(Isolate*) const {
    uint64_t value = 0;
    return TryValue(&value) ? Local<T>::New(value) : Local<T>();
  }

  template <class S, class = std::enable_if_t<std::is_base_of_v<T, S>>>
  void Reset(Isolate*, const Local<S>& that) {
    Adopt(that.IsEmpty() ? 0 : that.raw(), that.IsEmpty());
  }
};

/* The tagged word behind a `this` pointer: API objects have no fields. */
inline uint64_t Raw(const void* self) {
  return static_cast<uint64_t>(reinterpret_cast<uintptr_t>(self));
}

}  // namespace v8

#endif  // INCLUDE_V8_H_
