// Stage 0 of the Chromium port: the include gate.
//
// Blink's `bindings/core/v8/` includes V8's public headers by name, so a header
// the facade does not provide is a compile error before any symbol is
// considered. The bar is V8's own `include/` inventory, taken from a checkout
// rather than remembered: `tests/v8-headers.txt` carries the list and the
// commit it came from (V8 15.9.0 at the time of writing). This target reports
// which of those the facade provides, so the Stage 0 number is measured.
//
// It probes the facade's include directory at run time rather than with
// `__has_include`, which no compiler accepts outside a `#if`. Both paths are
// baked in by CMake, so the binary answers the same question from any working
// directory.
//
// This is the *header* census. The symbol census needs a translation unit built
// from Blink itself (`.notes/browser.md` §9.2).

#include <stdio.h>
#include <string.h>

#include <string>
#include <vector>

#ifndef SLAG_FACADE_INCLUDE_ROOT
#error "SLAG_FACADE_INCLUDE_ROOT must name crates/v8/cpp/include"
#endif
#ifndef SLAG_V8_HEADER_LIST
#error "SLAG_V8_HEADER_LIST must name tests/v8-headers.txt"
#endif

namespace {

struct Group {
  const char* name;
  size_t total;
  size_t present;
};

// The three trees V8's `include/` splits into. A header's group is its
// directory, so the report follows the checkout rather than an opinion.
const char* GroupOf(const char* path) {
  if (strncmp(path, "cppgc/", 6) == 0) {
    return "cppgc";
  }
  if (strncmp(path, "libplatform/", 12) == 0) {
    return "libplatform";
  }
  return "include";
}

bool Present(const char* path) {
  char full[1024];
  snprintf(full, sizeof(full), "%s/%s", SLAG_FACADE_INCLUDE_ROOT, path);
  FILE* file = fopen(full, "rb");
  if (file == nullptr) {
    return false;
  }
  fclose(file);
  return true;
}

std::vector<std::string> ReadList(const char* path) {
  std::vector<std::string> entries;
  FILE* file = fopen(path, "r");
  if (file == nullptr) {
    fprintf(stderr, "surface-census: cannot read %s\n", path);
    return entries;
  }
  char line[1024];
  while (fgets(line, sizeof(line), file) != nullptr) {
    size_t length = strlen(line);
    while (length > 0 && (line[length - 1] == '\n' || line[length - 1] == '\r')) {
      line[--length] = '\0';
    }
    if (length == 0 || line[0] == '#') {
      continue;
    }
    entries.emplace_back(line);
  }
  fclose(file);
  return entries;
}

}  // namespace

int main(int argc, char** argv) {
  // `--strict` turns the report into a gate: exit non-zero while any header is
  // missing, so the Stage 0 number can be watched to zero.
  bool strict = false;
  const char* list_path = SLAG_V8_HEADER_LIST;
  for (int index = 1; index < argc && argv[index] != nullptr; ++index) {
    if (strcmp(argv[index], "--strict") == 0) {
      strict = true;
    } else {
      list_path = argv[index];
    }
  }

  const std::vector<std::string> entries = ReadList(list_path);
  if (entries.empty()) {
    fprintf(stderr, "surface-census: the header list is empty\n");
    return 2;
  }

  Group groups[] = {{"include", 0, 0}, {"cppgc", 0, 0}, {"libplatform", 0, 0}};
  std::vector<const std::string*> missing;

  for (const std::string& entry : entries) {
    const char* group = GroupOf(entry.c_str());
    for (Group& candidate : groups) {
      if (strcmp(candidate.name, group) != 0) {
        continue;
      }
      ++candidate.total;
      if (Present(entry.c_str())) {
        ++candidate.present;
      } else {
        missing.push_back(&entry);
      }
      break;
    }
  }

  size_t total = 0;
  size_t present = 0;
  for (const Group& group : groups) {
    printf("%-12s %2zu/%2zu\n", group.name, group.present, group.total);
    total += group.total;
    present += group.present;
  }

  printf("\nmissing (%zu):\n", missing.size());
  for (const std::string* entry : missing) {
    printf("  %s\n", entry->c_str());
  }
  printf("\nsurface-census: %zu/%zu headers present\n", present, total);

  if (strict && present != total) {
    return 1;
  }
  return 0;
}
