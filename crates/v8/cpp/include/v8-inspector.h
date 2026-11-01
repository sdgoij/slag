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

  /* Connect a session to `contextGroupId`'s context.
   *
   * Answers null when the group is unknown, already has a session, or holds
   * more than one context. The session is per context today, and a Blink group
   * is a frame: one main world plus an isolated world per extension, each a
   * `v8::Context`. Making the session group-wide is what that limitation costs
   * (`.notes/chromium-facade.md` section 14.3). */
  virtual std::unique_ptr<V8InspectorSession> connect(
      int contextGroupId, Channel* channel,
      StringView state = StringView()) = 0;
};

}  // namespace v8_inspector

#endif  // INCLUDE_V8_INSPECTOR_H_
