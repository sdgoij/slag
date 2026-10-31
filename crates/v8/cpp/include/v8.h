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
#include <vector>

#include "slag_a0.h"

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

class Value {
 public:
  bool IsUndefined() const;
  bool IsString() const;
  Maybe<uint32_t> Uint32Value(Local<Context> context) const;
  Maybe<int32_t> Int32Value(Local<Context> context) const;
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
   * engine for the object's lifetime, like a template. */
  void SetAccessor(Local<Context> context, Local<Name> name,
                   AccessorNameGetterCallback getter,
                   AccessorNameSetterCallback setter = nullptr,
                   Local<Value> data = Local<Value>());

  /* The own-property predicates (`Object::HasRealNamedProperty` and its
   * siblings): no interceptor and no prototype walk. `HasRealIndexedProperty`
   * asks about an array index; the other two about a name. */
  Maybe<bool> HasRealNamedProperty(Local<Context> context, Local<Name> key);
  Maybe<bool> HasRealIndexedProperty(Local<Context> context, uint32_t index);
  Maybe<bool> HasRealNamedCallbackProperty(Local<Context> context,
                                           Local<Name> key);

  /* The C++ object this wrapper names. V8 requires its `T` to be a cppgc
   * `GarbageCollectedMixin`; the facade has no cppgc, so there is no such
   * constraint and nothing is checked. */
  template <class T>
  T* Wrappable() const {
    return static_cast<T*>(
        GetAlignedPointerFromInternalField(kWrappableFieldIndex));
  }
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

class Number : public Value {
 public:
  static Local<Number> New(Isolate* isolate, double value);
};

/* A property key: a `String` or a `Symbol` (`v8::Name`). Nothing here makes a
 * symbol yet, but the type is what `GetRealNamedProperty` takes, and a string
 * is a name, so the hierarchy is the one a host's code assumes. */
class Name : public Value {};

class Integer : public Number {
 public:
  static Local<Integer> New(Isolate* isolate, int32_t value);
};

class String : public Name {
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

/* A boolean value (`v8::Boolean`): the info a deleter answers through. */
class Boolean : public Value {
 public:
  static Local<Boolean> New(Isolate* isolate, bool value);
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
                       uint64_t this_value, const uint64_t* args, int length)
      : isolate_(isolate),
        call_(call),
        this_value_(this_value),
        args_(args),
        length_(length),
        return_value_(slag_undefined()) {}

  Isolate* GetIsolate() const { return isolate_; }
  int Length() const { return length_; }
  Local<T> operator[](int index) const { return Local<T>::New(args_[index]); }
  Local<T> This() const { return Local<T>::New(this_value_); }
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
                   Local<Value> data = Local<Value>());

  /* One installed accessor: the name and data are retained for the template's
   * lifetime (templates are not freed), and their bits are kept so the
   * trampolines do not have to make a handle per access. `Global` is defined
   * later in this header, so the retention is the ABI's root registry. */
  struct Accessor {
    uint64_t name_bits = 0;
    uint64_t data_bits = 0;
    AccessorNameGetterCallback getter = nullptr;
    AccessorNameSetterCallback setter = nullptr;
  };
  const std::deque<Accessor>& accessors() const { return accessors_; }

  /* Create an instance of this template: an ordinary object with the template's
   * internal fields, its host functions, and `IsApiWrapper` set. A template with
   * a handler makes an object whose property access goes to it. */
  MaybeLocal<Object> NewInstance(Local<Context> context);

 private:
  std::vector<Entry> entries_;
  std::deque<Accessor> accessors_;
  int internal_fields_ = 0;
  NamedPropertyHandlerConfiguration named_handler_;
  IndexedPropertyHandlerConfiguration indexed_handler_;
  bool has_named_handler_ = false;
  bool has_indexed_handler_ = false;
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

  static HandleSlot* CreateHandle(uint64_t value, bool rooted);

 private:
  std::size_t mark_;
};

template <class T>
Local<T> Local<T>::New(uint64_t value) {
  return Local<T>(
      HandleScope::CreateHandle(value, std::is_base_of_v<Value, T>));
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
    return empty_ ? Local<T>() : Local<T>::New(value_);
  }

  void Reset() { Release(); }
  void Reset(Isolate*, Local<T> local) {
    Release();
    Hold(local);
  }

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
    other.value_ = 0;
    other.empty_ = true;
  }

  void Release() {
    if (!empty_ && kEngineValue) slag_root_release(value_);
    value_ = 0;
    empty_ = true;
  }

  uint64_t value_ = 0;
  bool empty_ = true;
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
