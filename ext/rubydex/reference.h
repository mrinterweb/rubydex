#ifndef RUBYDEX_REFERENCE_H
#define RUBYDEX_REFERENCE_H

#include "ruby.h"
#include "rustbindings.h"

extern VALUE cReference;
extern VALUE cConstantReference;
extern VALUE cUnresolvedConstantReference;
extern VALUE cResolvedConstantReference;
extern VALUE cMethodReference;

VALUE rdxi_method_reference_new(VALUE graph_obj, uint64_t id, VALUE cached_name, uint64_t cached_epoch);

void rdxi_initialize_reference(VALUE mRubydex);

#endif // RUBYDEX_REFERENCE_H
