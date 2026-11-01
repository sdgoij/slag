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
  return reinterpret_cast<v8::Context*>(static_cast<uintptr_t>(handle.raw()));
}

class InspectorImpl;

/* One context group: the engine session that debugs it and the contexts that
 * joined. A group is a frame in Chromium, so its worlds are the contexts. */
struct GroupState {
  SlagInspector* slag = nullptr;
  std::vector<v8::Context*> contexts;
  /* One client per group: two sessions would share one engine's debug points. */
  bool connected = false;
};

/* One client's session over a group: its channel, and the protocol it drives. */
class InspectorSessionImpl final : public V8InspectorSession {
 public:
  InspectorSessionImpl(InspectorImpl* owner, SlagInspector* slag,
                       int context_group_id, V8Inspector::Channel* channel);
  ~InspectorSessionImpl() override;

  void dispatchProtocolMessage(StringView message) override;
  std::vector<uint8_t> state() override;
  void stop() override;
  void resume(bool setTerminateOnResume) override;
  EvaluateResult evaluate(v8::Local<v8::Context> context, StringView expression,
                          bool includeCommandLineAPI) override;
  void pollEvents() override;

  int context_group_id() const { return context_group_id_; }

 private:
  /* Send the messages a call produced to the channel. A dispatch's last
   * message is the response to its command and the rest are events; a poll's
   * are all events. */
  void Deliver(const SlagMessages& messages, int64_t call_id,
               bool answers_a_command);

  InspectorImpl* owner_;
  SlagInspector* slag_;
  int context_group_id_;
  V8Inspector::Channel* channel_;
};

class InspectorImpl final : public V8Inspector {
 public:
  InspectorImpl(v8::Isolate* isolate, V8InspectorClient* client)
      : isolate_(isolate), client_(client) {}

  ~InspectorImpl() override {
    for (InspectorSessionImpl* session : sessions_) session->stop();
    for (auto& entry : groups_) {
      if (entry.second.slag != nullptr) slag_inspector_free(entry.second.slag);
    }
  }

  InspectorImpl(const InspectorImpl&) = delete;
  InspectorImpl& operator=(const InspectorImpl&) = delete;

  void contextCreated(const V8ContextInfo& info) override {
    v8::Context* context = ContextOf(info.context);
    if (context == nullptr) return;
    GroupState& group = groups_[info.contextGroupId];
    if (group.slag == nullptr && slag_inspector_new(&group.slag) != 0) return;
    // A context belongs to one group, so joining twice is a bug in the caller
    // rather than something to paper over: the session would see the context
    // in two places.
    if (slag_inspector_add_context(group.slag, context->slag(), nullptr) == 0) {
      group.contexts.push_back(context);
    }
  }

  void contextDestroyed(v8::Local<v8::Context> handle) override {
    v8::Context* context = ContextOf(handle);
    if (context == nullptr) return;
    for (auto& entry : groups_) {
      GroupState& group = entry.second;
      auto found = std::find(group.contexts.begin(), group.contexts.end(),
                             context);
      if (found == group.contexts.end()) continue;
      group.contexts.erase(found);
      slag_inspector_remove_context(group.slag, context->slag());
      return;
    }
  }

  void resetContextGroup(int contextGroupId) override {
    auto found = groups_.find(contextGroupId);
    if (found == groups_.end()) return;
    for (InspectorSessionImpl* session : sessions_) {
      if (session->context_group_id() == contextGroupId) session->stop();
    }
    if (found->second.slag != nullptr) slag_inspector_free(found->second.slag);
    groups_.erase(found);
  }

  std::unique_ptr<V8InspectorSession> connect(int contextGroupId,
                                              Channel* channel,
                                              StringView /*state*/) override {
    if (channel == nullptr) return nullptr;
    auto found = groups_.find(contextGroupId);
    if (found == groups_.end() || found->second.slag == nullptr) return nullptr;
    if (found->second.connected) return nullptr;
    if (slag_inspector_open(found->second.slag) != 0) return nullptr;
    found->second.connected = true;
    return std::make_unique<InspectorSessionImpl>(this, found->second.slag,
                                                  contextGroupId, channel);
  }

  void AddSession(InspectorSessionImpl* session) {
    sessions_.push_back(session);
  }

  void RemoveSession(InspectorSessionImpl* session) {
    sessions_.erase(std::remove(sessions_.begin(), sessions_.end(), session),
                    sessions_.end());
  }

  /* A stopped session frees its group for the next client. */
  void SessionStopped(int contextGroupId) {
    auto found = groups_.find(contextGroupId);
    if (found != groups_.end()) found->second.connected = false;
  }

 private:
  v8::Isolate* isolate_;
  V8InspectorClient* client_;
  std::map<int, GroupState> groups_;
  std::vector<InspectorSessionImpl*> sessions_;
};

InspectorSessionImpl::InspectorSessionImpl(InspectorImpl* owner,
                                           SlagInspector* slag,
                                           int context_group_id,
                                           V8Inspector::Channel* channel)
    : owner_(owner),
      slag_(slag),
      context_group_id_(context_group_id),
      channel_(channel) {
  owner_->AddSession(this);
}

InspectorSessionImpl::~InspectorSessionImpl() {
  stop();
  owner_->RemoveSession(this);
}

void InspectorSessionImpl::dispatchProtocolMessage(StringView message) {
  if (slag_ == nullptr) return;
  const std::string text = Utf8Of(message);
  const auto* bytes = reinterpret_cast<const uint8_t*>(text.data());
  const int64_t call_id = slag_inspector_call_id(bytes, text.size());
  SlagMessages messages{};
  if (slag_inspector_dispatch(slag_, bytes, text.size(), &messages) != 0) {
    return;
  }
  Deliver(messages, call_id, /*answers_a_command=*/true);
  slag_messages_free(&messages);
  // A command can leave a run stopped (a resume that reaches another
  // breakpoint, or a step), and a stop is an event, not an answer.
  pollEvents();
}

void InspectorSessionImpl::pollEvents() {
  if (slag_ == nullptr) return;
  SlagMessages messages{};
  if (slag_inspector_poll(slag_, &messages) != 0) return;
  Deliver(messages, -1, /*answers_a_command=*/false);
  slag_messages_free(&messages);
}

std::vector<uint8_t> InspectorSessionImpl::state() {
  return std::vector<uint8_t>();
}

void InspectorSessionImpl::stop() {
  if (slag_ == nullptr) return;
  slag_inspector_close(slag_);
  slag_ = nullptr;
  owner_->SessionStopped(context_group_id_);
}

void InspectorSessionImpl::resume(bool /*setTerminateOnResume*/) {
  if (slag_ == nullptr) return;
  SlagMessages messages{};
  if (slag_inspector_resume(slag_, &messages) == 0) {
    Deliver(messages, -1, /*answers_a_command=*/false);
    slag_messages_free(&messages);
  }
  // A resume can stop again (a breakpoint further on), and that stop is an
  // event the client learns about by polling.
  pollEvents();
}

V8InspectorSession::EvaluateResult InspectorSessionImpl::evaluate(
    v8::Local<v8::Context> context, StringView expression,
    bool /*includeCommandLineAPI*/) {
  EvaluateResult result;
  v8::Context* target = ContextOf(context);
  if (slag_ == nullptr || target == nullptr) return result;
  const std::string text = Utf8Of(expression);
  uint64_t value = 0;
  const int32_t status =
      slag_inspector_evaluate(slag_, target->slag(),
                              reinterpret_cast<const uint8_t*>(text.data()),
                              text.size(), &value);
  if (status == 0) {
    result.type = EvaluateResult::ResultType::kSuccess;
    result.value = v8::Local<v8::Value>::New(value);
    return result;
  }
  // 2 is the engine refusing the context (it is not in the group), which is
  // `kNotRun`; 1 is a throw, and a throw is the result here, so the pending
  // error is consumed the way `TryCatch`'s scope consumes one.
  if (status != 1) return result;
  uint64_t thrown = 0;
  const bool has_value = slag_pending_exception(target->slag(), &thrown) != 0;
  slag_clear_error(target->slag());
  result.type = EvaluateResult::ResultType::kException;
  result.value = v8::Local<v8::Value>::New(has_value ? thrown : 0);
  return result;
}

void InspectorSessionImpl::Deliver(const SlagMessages& messages,
                                   int64_t call_id, bool answers_a_command) {
  for (size_t index = 0; index < messages.len; ++index) {
    const SlagProtocolMessage& message = messages.items[index];
    const bool is_answer = answers_a_command && index + 1 == messages.len;
    auto buffer = StringBuffer::create(StringView(message.ptr, message.len));
    if (is_answer) {
      channel_->sendResponse(static_cast<int>(call_id), std::move(buffer));
    } else {
      channel_->sendNotification(std::move(buffer));
    }
  }
  channel_->flushProtocolNotifications();
}

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
