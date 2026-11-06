#ifndef INCLUDE_V8_FORWARD_H_
#define INCLUDE_V8_FORWARD_H_
/* V8's `v8-forward.h` is the cheap forward-declaration set a host includes to
 * name types in a header. This facade has no split declarations, so the whole
 * of `v8.h` is what there is: correct, at the compile-time cost the split
 * exists to avoid. Splitting it is header work, not engine work. */
#include "v8.h"
#endif
