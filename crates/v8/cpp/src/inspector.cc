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

StringView ViewOf(const std::string& text) {
  return StringView(reinterpret_cast<const uint8_t*>(text.data()), text.size());
}

/* The UTF-8 text a tagged engine string answers, empty for "no string". */
std::string Utf8OfValue(uint64_t bits) {
  if (bits == 0) return std::string();
  const int64_t length = slag_value_to_utf8(bits, nullptr, 0);
  if (length <= 0) return std::string();
  std::string text(static_cast<size_t>(length) + 1, '\0');
  slag_value_to_utf8(bits, text.data(), text.size());
  text.resize(static_cast<size_t>(length));
  return text;
}

/* One captured frame, owning its strings: `V8StackFrame` reports views into
 * them, so the trace has to keep them. */
struct TraceFrame {
  std::string function_name;
  std::string url;
  int line = 0;
  int column = 0;
  int script_id = 0;
};

class V8StackTraceImpl final : public V8StackTrace {
 public:
  explicit V8StackTraceImpl(std::vector<TraceFrame> frames)
      : frames_(std::move(frames)) {}

  bool isEmpty() const override { return frames_.empty(); }

  StringView topSourceURL() const override {
    return frames_.empty() ? StringView() : ViewOf(frames_.front().url);
  }

  int topLineNumber() const override {
    return frames_.empty() ? 0 : frames_.front().line;
  }

  int topColumnNumber() const override {
    return frames_.empty() ? 0 : frames_.front().column;
  }

  int topScriptId() const override {
    return frames_.empty() ? 0 : frames_.front().script_id;
  }

  StringView topFunctionName() const override {
    return frames_.empty() ? StringView() : ViewOf(frames_.front().function_name);
  }

  StringView firstNonEmptySourceURL() const override {
    for (const TraceFrame& frame : frames_) {
      if (!frame.url.empty()) return ViewOf(frame.url);
    }
    return StringView();
  }

  std::vector<V8StackFrame> frames() const override {
    std::vector<V8StackFrame> out;
    out.reserve(frames_.size());
    for (const TraceFrame& frame : frames_) {
      V8StackFrame view;
      view.sourceURL = ViewOf(frame.url);
      view.functionName = ViewOf(frame.function_name);
      view.lineNumber = frame.line;
      view.columnNumber = frame.column;
      view.scriptId = frame.script_id;
      out.push_back(view);
    }
    return out;
  }

  std::unique_ptr<V8StackTrace> clone() override {
    return std::make_unique<V8StackTraceImpl>(frames_);
  }

  std::unique_ptr<StringBuffer> toString() const override {
    std::string out;
    for (const TraceFrame& frame : frames_) {
      out += "    at ";
      if (!frame.function_name.empty()) {
        out += frame.function_name;
        if (!frame.url.empty()) {
          out += " (";
          out += frame.url;
          out += ':';
          out += std::to_string(frame.line);
          out += ':';
          out += std::to_string(frame.column);
          out += ')';
        }
      } else if (!frame.url.empty()) {
        out += frame.url;
        out += ':';
        out += std::to_string(frame.line);
        out += ':';
        out += std::to_string(frame.column);
      } else {
        out += "<anonymous>";
      }
      out += '\n';
    }
    return StringBuffer::create(ViewOf(out));
  }

 private:
  std::vector<TraceFrame> frames_;
};

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

  /* Send messages the inspector produced outside a dispatch (an exception
   * event) to the channel, taking the list. */
  void Notify(SlagMessages messages);

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
                                              StringView /*state*/,
                                              V8EmbedderState embedderState) override {
    if (channel == nullptr) return nullptr;
    auto found = groups_.find(contextGroupId);
    if (found == groups_.end() || found->second.slag == nullptr) return nullptr;
    if (found->second.connected) return nullptr;
    if (slag_inspector_open(found->second.slag) != 0) return nullptr;
    // The browser's own breakpoints are installed before the session answers
    // anything, which is what lets a client attach to a page already running.
    for (const V8URLBreakpoint& breakpoint : embedderState.urlBreakpoints) {
      if (breakpoint.selectorType != V8URLBreakpoint::kUrl) continue;
      if (breakpoint.condition.length() != 0) continue;
      const std::string url = Utf8Of(breakpoint.selector);
      const int32_t status = slag_inspector_embedder_breakpoint(
          found->second.slag, reinterpret_cast<const uint8_t*>(url.data()),
          url.size(), static_cast<uint32_t>(breakpoint.lineNumber),
          static_cast<uint32_t>(breakpoint.columnNumber.value_or(0)));
      // 3 is "no script with that URL yet", which a browser-originating
      // breakpoint outlives; anything else is a back-end failure and the
      // connect fails with it rather than pretending to have them installed.
      if (status != 0 && status != 3) return nullptr;
    }
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

  std::unique_ptr<V8StackTrace> captureStackTrace(bool fullStack) override;
  unsigned exceptionThrown(v8::Local<v8::Context> context, StringView message,
                           v8::Local<v8::Value> exception,
                           StringView detailedMessage, StringView url,
                           unsigned lineNumber, unsigned columnNumber,
                           std::unique_ptr<V8StackTrace> stackTrace,
                           int scriptId) override;
  void exceptionRevoked(v8::Local<v8::Context> context, unsigned exceptionId,
                        StringView message) override;

 private:
  /* The group a context belongs to, and its state, or a null state. */
  GroupState* GroupOf(v8::Context* context, int* context_group_id) {
    for (auto& entry : groups_) {
      for (v8::Context* member : entry.second.contexts) {
        if (member != context) continue;
        *context_group_id = entry.first;
        return &entry.second;
      }
    }
    return nullptr;
  }

  /* The live client session of a group, or null. */
  InspectorSessionImpl* SessionFor(int context_group_id) {
    for (InspectorSessionImpl* session : sessions_) {
      if (session->context_group_id() == context_group_id) return session;
    }
    return nullptr;
  }
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

void InspectorSessionImpl::Notify(SlagMessages messages) {
  Deliver(messages, -1, /*answers_a_command=*/false);
  slag_messages_free(&messages);
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

std::unique_ptr<V8StackTrace> InspectorImpl::captureStackTrace(bool fullStack) {
  v8::Isolate* isolate = v8::Isolate::GetCurrent();
  if (isolate == nullptr) return nullptr;
  v8::Context* context = ContextOf(isolate->GetCurrentContext());
  if (context == nullptr) return nullptr;
  // Inside a host callback the engine is borrowed by the run, so the stack is
  // read through that call's own window; anywhere else the context reads it.
  const void* window = slag_callback_token();
  const auto frame_count = [&]() {
    return window != nullptr
               ? slag_callback_stack_frame_count(context->slag(), window)
               : slag_stack_frame_count(context->slag());
  };
  const auto frame_at = [&](size_t index, SlagStackFrame* out) {
    return window != nullptr
               ? slag_callback_stack_frame(context->slag(), window, index, out)
               : slag_stack_frame(context->slag(), index, out);
  };
  const int32_t available = frame_count();
  // V8 stops a short capture at its API boundary; the engine's frames carry
  // that flag but the ABI does not expose it, so the difference here is the
  // frame count.
  const int32_t limit = fullStack ? 200 : 10;
  std::vector<TraceFrame> frames;
  frames.reserve(static_cast<size_t>(available < limit ? available : limit));
  for (int32_t index = 0;
       index < available && static_cast<int32_t>(frames.size()) < limit;
       ++index) {
    SlagStackFrame frame{};
    if (frame_at(static_cast<size_t>(index), &frame) != 0) break;
    frames.push_back(TraceFrame{Utf8OfValue(frame.function_name),
                                Utf8OfValue(frame.script_name), frame.line,
                                frame.column,
                                static_cast<int>(frame.script_id)});
  }
  return std::make_unique<V8StackTraceImpl>(std::move(frames));
}

unsigned InspectorImpl::exceptionThrown(
    v8::Local<v8::Context> handle, StringView /*message*/,
    v8::Local<v8::Value> exception, StringView detailedMessage, StringView url,
    unsigned lineNumber, unsigned columnNumber,
    std::unique_ptr<V8StackTrace> stackTrace, int /*scriptId*/) {
  v8::Context* context = ContextOf(handle);
  if (context == nullptr) return 0;
  int context_group_id = 0;
  GroupState* group = GroupOf(context, &context_group_id);
  if (group == nullptr || group->slag == nullptr) return 0;
  InspectorSessionImpl* session = SessionFor(context_group_id);
  if (session == nullptr) return 0;

  const std::string text = Utf8Of(detailedMessage);
  const std::string where = Utf8Of(url);
  // A frame reports views, so the strings are copied first and only then
  // pointed at; the vectors do not grow after the pointers are taken.
  const std::vector<V8StackFrame> captured =
      stackTrace ? stackTrace->frames() : std::vector<V8StackFrame>();
  std::vector<std::pair<std::string, std::string>> named;
  named.reserve(captured.size());
  for (const V8StackFrame& frame : captured) {
    named.emplace_back(Utf8Of(frame.functionName), Utf8Of(frame.sourceURL));
  }
  std::vector<SlagExceptionFrame> frames;
  frames.reserve(captured.size());
  for (size_t index = 0; index < captured.size(); ++index) {
    const std::string& function_name = named[index].first;
    const std::string& frame_url = named[index].second;
    frames.push_back(SlagExceptionFrame{
        reinterpret_cast<const uint8_t*>(function_name.data()),
        function_name.size(), reinterpret_cast<const uint8_t*>(frame_url.data()),
        frame_url.size(), static_cast<uint32_t>(captured[index].lineNumber),
        static_cast<uint32_t>(captured[index].columnNumber)});
  }
  const bool is_error =
      !exception.IsEmpty() &&
      (slag_value_type_mask(context->slag(), exception.raw()) &
       SLAG_VALUE_NATIVE_ERROR) != 0;
  SlagException report{};
  report.message = reinterpret_cast<const uint8_t*>(text.data());
  report.message_len = text.size();
  report.url = reinterpret_cast<const uint8_t*>(where.data());
  report.url_len = where.size();
  report.line = lineNumber;
  report.column = columnNumber;
  report.is_error = is_error ? 1 : 0;
  report.frames = frames.empty() ? nullptr : frames.data();
  report.frame_count = frames.size();

  uint32_t exception_id = 0;
  SlagMessages messages{};
  if (slag_inspector_exception(group->slag, &report, &exception_id, &messages) !=
      0) {
    return 0;
  }
  session->Notify(messages);
  return exception_id;
}

void InspectorImpl::exceptionRevoked(v8::Local<v8::Context> handle,
                                     unsigned exceptionId, StringView message) {
  v8::Context* context = ContextOf(handle);
  if (context == nullptr) return;
  int context_group_id = 0;
  GroupState* group = GroupOf(context, &context_group_id);
  if (group == nullptr || group->slag == nullptr) return;
  InspectorSessionImpl* session = SessionFor(context_group_id);
  if (session == nullptr) return;
  const std::string reason = Utf8Of(message);
  SlagMessages messages{};
  if (slag_inspector_exception_revoked(
          group->slag, exceptionId,
          reinterpret_cast<const uint8_t*>(reason.data()), reason.size(),
          &messages) != 0) {
    return;
  }
  session->Notify(messages);
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
V8StackTrace::~V8StackTrace() = default;
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
