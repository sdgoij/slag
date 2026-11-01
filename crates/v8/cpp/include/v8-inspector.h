#ifndef INCLUDE_V8_INSPECTOR_H_
#define INCLUDE_V8_INSPECTOR_H_

/*
 * The `v8-inspector.h` surface a client connects a DevTools session through,
 * over the protocol session in `crates/inspector`.
 *
 * This is not a copy of V8's header. It carries the connection and the
 * protocol: a client creates an inspector, announces its contexts, connects a
 * session with a channel, and from then on the bytes it sends are the Chrome
 * DevTools Protocol, answered by the Rust session. What V8's header also
 * carries for Blink's page domains is deliberately absent, because it is
 * generated from `js_protocol.pdl` by `inspector_protocol` and this facade has
 * no generated protocol tree: `wrapObject`/`unwrapObject` (a
 * `protocol::Runtime::API::RemoteObject`), `searchInTextByLines`,
 * `supportedDomains`, `deepSerialize`, `V8StackTrace`, and the console and
 * async-task callbacks. `.notes/chromium-facade.md` section 14.3 records each
 * of them with what it costs.
 *
 * The pause loop is the client's. V8 calls `V8InspectorClient::
 * runMessageLoopOnPause` itself and only returns to the run after the debugger
 * resumes; here the host runs the script, sees the pause, polls the session for
 * its events and dispatches what the client sends, because a resumed run
 * completes inside the resume (`Script::Run`'s engine call has already
 * returned by then).
 */

#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "v8.h"

namespace v8_inspector {

class V8Inspector;
class V8InspectorSession;

/* A borrowed string as the inspector hands one over: UTF-8 bytes or UTF-16
 * units, owned by the caller for the length of the call. */
class StringView {
 public:
  StringView() : length_(0), characters8_(nullptr), is8_bit_(true) {}
  StringView(const uint8_t* characters, size_t length)
      : length_(length), characters8_(characters), is8_bit_(true) {}
  StringView(const uint16_t* characters, size_t length)
      : length_(length), characters16_(characters), is8_bit_(false) {}

  bool is8Bit() const { return is8_bit_; }
  size_t length() const { return length_; }
  const uint8_t* characters8() const { return characters8_; }
  const uint16_t* characters16() const { return characters16_; }

 private:
  size_t length_;
  union {
    const uint8_t* characters8_;
    const uint16_t* characters16_;
  };
  bool is8_bit_;
};

/* An owned string, the answer to anything the inspector builds: a protocol
 * message, a description. */
class StringBuffer {
 public:
  virtual ~StringBuffer() = default;
  virtual StringView string() const = 0;
  /* `StringBuffer::create` copies `text`, converting UTF-16 to UTF-8 because
   * the protocol session speaks UTF-8. */
  static std::unique_ptr<StringBuffer> create(StringView text);
};

/* One frame of a [`V8StackTrace`]: the views point into the trace that
 * produced them, so they are valid while it lives. */
struct V8StackFrame {
  StringView sourceURL;
  StringView functionName;
  int lineNumber = 0;
  int columnNumber = 0;
  int scriptId = 0;
};

/* A stack captured where an error value was created. A frame is gone by the
 * time the error reaches the embedder, so the host captures here and reports
 * later (`V8Inspector::exceptionThrown`). */
class V8StackTrace {
 public:
  virtual ~V8StackTrace();
  virtual bool isEmpty() const = 0;
  virtual StringView topSourceURL() const = 0;
  virtual int topLineNumber() const = 0;
  virtual int topColumnNumber() const = 0;
  virtual int topScriptId() const = 0;
  virtual StringView topFunctionName() const = 0;
  virtual StringView firstNonEmptySourceURL() const = 0;
  virtual std::vector<V8StackFrame> frames() const = 0;
  /* A copy, for a host that keeps one of its own (`SourceLocation::Clone`).
   * The async chain V8 would drop here does not exist in this engine. */
  virtual std::unique_ptr<V8StackTrace> clone() = 0;
  /* The `Error.stack` rendering: one `    at ...` line per frame. */
  virtual std::unique_ptr<StringBuffer> toString() const = 0;
};

/* A breakpoint the browser already knows about, handed to a session as it
 * connects (`V8EmbedderState`). Chromium keeps these in the browser process, so
 * a renderer that attaches after a navigation gets them without the frontend
 * re-sending anything.
 *
 * Only `kUrl` is honoured: `kUrlRegex` would need a regular expression engine the
 * session does not have, and `kScriptHash` is Chromium's hash of the source
 * rather than the identity this engine derives. A `condition` is skipped for a
 * sharper reason: honouring it means running code at every stop, which the
 * session does not do yet. Skipping is deliberate: a breakpoint that fires more
 * often than it was asked to is worse than one that does not fire. */
struct V8URLBreakpoint {
  StringView breakpointId;
  int lineNumber = 0;
  std::optional<int> columnNumber;

  enum SelectorType { kUrl, kUrlRegex, kScriptHash };
  SelectorType selectorType = kUrl;
  StringView selector;

  StringView condition;
};

struct V8EmbedderState {
  std::vector<V8URLBreakpoint> urlBreakpoints;
};

/* How a context joins an inspector: the embedder's group id (Chromium groups
 * by frame) and the names the client shows for it. */
class V8ContextInfo {
 public:
  V8ContextInfo(v8::Local<v8::Context> context, int contextGroupId,
                StringView humanReadableName)
      : context(context),
        contextGroupId(contextGroupId),
        humanReadableName(humanReadableName) {}

  v8::Local<v8::Context> context;
  int contextGroupId;
  StringView humanReadableName;
  StringView origin;
  StringView auxData;
  bool hasMemoryOnConsole = false;
};

/* The embedder's half of the inspector. Nothing calls back into it yet: the
 * message loop, console routing and value serialization are the calls V8 makes
 * here, and the host drives the pause loop itself for now (see the file
 * comment). */
class V8InspectorClient {
 public:
  virtual ~V8InspectorClient();
};

class V8InspectorSession {
 public:
  virtual ~V8InspectorSession();

  /* Feed one protocol message. The events it caused and its response go to the
   * channel the session was connected with; nothing is answered here. */
  virtual void dispatchProtocolMessage(StringView message) = 0;

  /* The state to hand back on a later connect. Empty: breakpoints live in this
   * session rather than the engine, so a reattach starts with none. */
  virtual std::vector<uint8_t> state() = 0;

  /* Disconnect: a parked run continues and the engine detaches. */
  virtual void stop() = 0;

  /* Continue a parked run (the engine call behind `Debugger.resume`). A client
   * asking for `setTerminateOnResume` gets a plain resume: this engine's resume
   * always runs the script to its end or its next stop, and the value the run
   * completes with belongs to the `Script::Run` that started it. */
  virtual void resume(bool setTerminateOnResume = false) = 0;

  struct EvaluateResult {
    enum class ResultType { kNotRun, kSuccess, kException };
    ResultType type = ResultType::kNotRun;
    v8::Local<v8::Value> value;
  };

  /* Evaluate `expression` in the session's context, which is how a host
   * injects what it installs for the command line. `kNotRun` answers a context
   * this session does not own. */
  virtual EvaluateResult evaluate(v8::Local<v8::Context> context,
                                  StringView expression,
                                  bool includeCommandLineAPI = false) = 0;

  /* The events a run produced so far, sent to the channel: a
   * `Debugger.scriptParsed` a compile queued, and a `Debugger.paused` for a
   * stop that has not been announced.
   *
   * Not part of V8's header. V8 announces a pause from inside the run, because
   * it owns the pause loop; this facade leaves the loop to the host (see the
   * file comment), so the drain is a call the host makes after a run stops.
   * The rest of a host's loop is `dispatchProtocolMessage` with what the client
   * answers. */
  virtual void pollEvents() = 0;

  /* Whether the session handles `method`, which is how a host routes a message
   * between the debugger and its own agents. */
  static bool canDispatchMethod(StringView method);
};

class V8Inspector {
 public:
  static std::unique_ptr<V8Inspector> create(v8::Isolate* isolate,
                                            V8InspectorClient* client);
  virtual ~V8Inspector();

  /* A context joined `contextGroupId`. */
  virtual void contextCreated(const V8ContextInfo& info) = 0;
  /* A context is going away. */
  virtual void contextDestroyed(v8::Local<v8::Context> context) = 0;
  /* Every context of the group is gone. */
  virtual void resetContextGroup(int contextGroupId) = 0;

  /* Capture the running stack, which is what a host does where it creates an
   * error value. `fullStack` caps the capture (200 frames against 10); V8 also
   * stops a short capture at its API boundary, and this engine's frames carry
   * that flag but the ABI does not expose it yet, so the difference here is the
   * frame count. Answers null when no context is entered. */
  virtual std::unique_ptr<V8StackTrace> captureStackTrace(bool fullStack) = 0;

  /* Report an uncaught exception: the `Runtime.exceptionThrown` event, and the
   * id `exceptionRevoked` names back. Answers 0 when no client is connected or
   * none enabled `Runtime`, which is the protocol's own rule.
   *
   * `scriptId` is V8's own script numbering, which this facade does not mint: a
   * session resolves the script from the URL the host compiled it under. */
  virtual unsigned exceptionThrown(v8::Local<v8::Context> context,
                                   StringView message,
                                   v8::Local<v8::Value> exception,
                                   StringView detailedMessage, StringView url,
                                   unsigned lineNumber, unsigned columnNumber,
                                   std::unique_ptr<V8StackTrace> stackTrace,
                                   int scriptId) = 0;

  /* The error is no longer uncaught (a handler took it). */
  virtual void exceptionRevoked(v8::Local<v8::Context> context,
                                unsigned exceptionId, StringView message) = 0;

  /* Where a session's messages go: `sendResponse` for the answer to a command,
   * `sendNotification` for an event. The channel outlives the session. */
  class Channel {
   public:
    virtual ~Channel();
    virtual void sendResponse(int callId,
                              std::unique_ptr<StringBuffer> message) = 0;
    virtual void sendNotification(std::unique_ptr<StringBuffer> message) = 0;
    virtual void flushProtocolNotifications() = 0;
  };

  /* Connect a session to `contextGroupId`'s contexts.
   *
   * Answers null when the group is unknown or already has a client. Every
   * context that joined the group is part of the session, which is what a
   * Blink group needs: a frame's main world and its isolated worlds are
   * separate `v8::Context`s of one group. Two clients are refused because the
   * engine's debug points and breakpoint set are per context and one session
   * owns them.
   *
   * `state` is the client's own persisted blob, which this facade does not
   * interpret; `embedder_state` is the browser's, and its URL breakpoints are
   * installed before the session answers anything. */
  virtual std::unique_ptr<V8InspectorSession> connect(
      int contextGroupId, Channel* channel, StringView state = StringView(),
      V8EmbedderState embedderState = {}) = 0;
};

}  // namespace v8_inspector

#endif  // INCLUDE_V8_INSPECTOR_H_
