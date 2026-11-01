#include "include/v8-inspector.h"

#include <algorithm>
#include <map>
#include <utility>

#include "include/slag_a0.h"

namespace v8_inspector {
namespace {

/* UTF-8 bytes for one code point. */
void AppendUtf8(std::string* out, uint32_t code) {
  if (code < 0x80) {
    out->push_back(static_cast<char>(code));
  } else if (code < 0x800) {
    out->push_back(static_cast<char>(0xC0 | (code >> 6)));
    out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
  } else if (code < 0x10000) {
    out->push_back(static_cast<char>(0xE0 | (code >> 12)));
    out->push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
  } else {
    out->push_back(static_cast<char>(0xF0 | (code >> 18)));
    out->push_back(static_cast<char>(0x80 | ((code >> 12) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
  }
}

/* The UTF-8 bytes of a `StringView`, which is what the protocol session takes.
 * A surrogate pair is combined, and a lone surrogate is replaced rather than
 * encoded as an invalid sequence. */
std::string Utf8Of(StringView text) {
  if (text.is8Bit()) {
    if (text.characters8() == nullptr) return std::string();
    return std::string(reinterpret_cast<const char*>(text.characters8()),
                       text.length());
  }
  std::string out;
  out.reserve(text.length());
  for (size_t index = 0; index < text.length(); ++index) {
    uint32_t code = text.characters16()[index];
    if (code >= 0xD800 && code <= 0xDBFF && index + 1 < text.length()) {
      const uint32_t low = text.characters16()[index + 1];
      if (low >= 0xDC00 && low <= 0xDFFF) {
        code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
        ++index;
      }
    }
    if (code >= 0xD800 && code <= 0xDFFF) code = 0xFFFD;
    AppendUtf8(&out, code);
  }
  return out;
}

class Utf8StringBuffer final : public StringBuffer {
 public:
  explicit Utf8StringBuffer(std::string text) : text_(std::move(text)) {}

  StringView string() const override {
    return StringView(reinterpret_cast<const uint8_t*>(text_.data()),
                      text_.size());
  }

 private:
  std::string text_;
};

/* The facade's context behind a handle. */
inline v8::Context* ContextOf(v8::Local<v8::Context> handle) {
  if (handle.IsEmpty()) return nullptr;
  return reinterpret_cast<v8::Context*>(
      static_cast<uintptr_t>(handle.raw()));
}

/* One session's channel, its context, and the protocol it drives. */
class InspectorSessionImpl final : public V8InspectorSession {
 public:
  InspectorSessionImpl(v8::Context* context, V8Inspector::Channel* channel)
      : context_(context), channel_(channel) {}

  ~InspectorSessionImpl() override { stop(); }

  void dispatchProtocolMessage(StringView message) override {
    if (context_ == nullptr) return;
    const std::string text = Utf8Of(message);
    const auto* bytes = reinterpret_cast<const uint8_t*>(text.data());
    const int64_t call_id = slag_inspector_call_id(bytes, text.size());
    SlagMessages messages{};
    if (slag_inspector_dispatch(context_->slag(), bytes, text.size(),
                                &messages) != 0) {
      return;
    }
    Deliver(messages, call_id, /*answers_a_command=*/true);
    slag_messages_free(&messages);
    // A command can leave the run stopped (a resume that reaches another
    // breakpoint, or a step), and a stop is an event, not an answer.
    pollEvents();
  }

  void pollEvents() override {
    if (context_ == nullptr) return;
    SlagMessages messages{};
    if (slag_inspector_poll(context_->slag(), &messages) != 0) return;
    Deliver(messages, -1, /*answers_a_command=*/false);
    slag_messages_free(&messages);
  }

  std::vector<uint8_t> state() override { return std::vector<uint8_t>(); }

  void stop() override {
    if (context_ == nullptr) return;
    slag_inspector_close(context_->slag());
    context_ = nullptr;
  }

  void resume(bool /*setTerminateOnResume*/) override {
    if (context_ == nullptr) return;
    SlagPause pause{};
    // Only a parked run continues, and a resume with nothing parked is not an
    // error (a client may resume after a stop it already handled).
    if (slag_debug_paused(context_->slag(), &pause) == 1) {
      uint64_t completion = 0;
      slag_debug_resume(context_->slag(), &completion);
    }
    pollEvents();
  }

  EvaluateResult evaluate(v8::Local<v8::Context> context,
                          StringView expression,
                          bool /*includeCommandLineAPI*/) override {
    EvaluateResult result;
    if (context_ == nullptr || ContextOf(context) != context_) return result;
    const std::string text = Utf8Of(expression);
    uint64_t value = 0;
    if (slag_inspector_evaluate(context_->slag(),
                                reinterpret_cast<const uint8_t*>(text.data()),
                                text.size(), &value) == 0) {
      result.type = EvaluateResult::ResultType::kSuccess;
      result.value = v8::Local<v8::Value>::New(value);
      return result;
    }
    result.type = EvaluateResult::ResultType::kException;
    // The thrown value is the result here, so the pending error is consumed:
    // a client that reads the exception must not leave the next operation
    // inheriting it (which is what `TryCatch`'s destructor does for its own
    // scope).
    uint64_t thrown = 0;
    const bool has_value =
        slag_pending_exception(context_->slag(), &thrown) != 0;
    slag_clear_error(context_->slag());
    result.value = v8::Local<v8::Value>::New(has_value ? thrown : 0);
    return result;
  }

 private:
  /* Send the messages a call produced to the channel. A dispatch's last
   * message is the response to its command and the rest are events; a poll's
   * are all events. */
  void Deliver(const SlagMessages& messages, int64_t call_id,
               bool answers_a_command) {
    for (size_t index = 0; index < messages.len; ++index) {
      const SlagProtocolMessage& message = messages.items[index];
      const bool is_answer =
          answers_a_command && index + 1 == messages.len;
      auto buffer = StringBuffer::create(
          StringView(message.ptr, message.len));
      if (is_answer) {
        channel_->sendResponse(static_cast<int>(call_id), std::move(buffer));
      } else {
        channel_->sendNotification(std::move(buffer));
      }
    }
    channel_->flushProtocolNotifications();
  }

  v8::Context* context_;
  V8Inspector::Channel* channel_;
};

class InspectorImpl final : public V8Inspector {
 public:
  InspectorImpl(v8::Isolate* isolate, V8InspectorClient* client)
      : isolate_(isolate), client_(client) {}

  void contextCreated(const V8ContextInfo& info) override {
    groups_[info.contextGroupId].push_back(info.context);
  }

  void contextDestroyed(v8::Local<v8::Context> context) override {
    for (auto& entry : groups_) {
      std::vector<v8::Local<v8::Context>>& contexts = entry.second;
      contexts.erase(std::remove(contexts.begin(), contexts.end(), context),
                     contexts.end());
    }
  }

  void resetContextGroup(int contextGroupId) override {
    groups_.erase(contextGroupId);
  }

  std::unique_ptr<V8InspectorSession> connect(int contextGroupId,
                                              Channel* channel,
                                              StringView /*state*/) override {
    if (channel == nullptr) return nullptr;
    auto found = groups_.find(contextGroupId);
    if (found == groups_.end() || found->second.size() != 1) return nullptr;
    v8::Context* context = ContextOf(found->second.front());
    if (context == nullptr) return nullptr;
    // A second session on one context would have to share the engine's single
    // debugger, and the protocol session owns it: the engine's breakpoint set
    // is per agent, so two clients would overwrite each other's.
    if (slag_inspector_open(context->slag()) != 0) return nullptr;
    return std::make_unique<InspectorSessionImpl>(context, channel);
  }

 private:
  v8::Isolate* isolate_;
  V8InspectorClient* client_;
  std::map<int, std::vector<v8::Local<v8::Context>>> groups_;
};

}  // namespace

std::unique_ptr<StringBuffer> StringBuffer::create(StringView text) {
  return std::make_unique<Utf8StringBuffer>(Utf8Of(text));
}

V8InspectorClient::~V8InspectorClient() = default;
V8InspectorSession::~V8InspectorSession() = default;
V8Inspector::Channel::~Channel() = default;
V8Inspector::~V8Inspector() = default;

bool V8InspectorSession::canDispatchMethod(StringView method) {
  const std::string text = Utf8Of(method);
  return slag_inspector_can_dispatch(
             reinterpret_cast<const uint8_t*>(text.data()), text.size()) != 0;
}

std::unique_ptr<V8Inspector> V8Inspector::create(v8::Isolate* isolate,
                                                V8InspectorClient* client) {
  return std::make_unique<InspectorImpl>(isolate, client);
}

}  // namespace v8_inspector
