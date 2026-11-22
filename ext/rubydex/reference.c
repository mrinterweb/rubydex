#include "reference.h"
#include "declaration.h"
#include "graph.h"
#include "handle.h"
#include "location.h"
#include "rustbindings.h"
#include "utils.h"

/*
 * RDoc parser workaround for https://github.com/ruby/rdoc/issues/1744:
 * mRubydex = rb_define_module("Rubydex")
 */

VALUE cReference;
VALUE cConstantReference;
VALUE cUnresolvedConstantReference;
VALUE cResolvedConstantReference;
VALUE cMethodReference;

static VALUE cDocument;

typedef struct {
    VALUE graph_obj;
    VALUE cached_name; // Qundef when uncached; frozen UTF-8 String valid while graph epoch matches
    uint64_t id;
    uint64_t cached_epoch;
} MethodReferenceData;

static void method_reference_mark(void *ptr) {
    if (ptr) {
        MethodReferenceData *data = (MethodReferenceData *)ptr;
        rb_gc_mark(data->graph_obj);
        if (data->cached_name != Qundef) {
            rb_gc_mark(data->cached_name);
        }
    }
}

static void method_reference_free(void *ptr) {
    if (ptr) {
        xfree(ptr);
    }
}

static const rb_data_type_t method_reference_type = {
    .wrap_struct_name = "RubydexMethodReference",
    .function = {
        .dmark = method_reference_mark,
        .dfree = method_reference_free,
        .dsize = NULL,
        .dcompact = NULL,
    },
    .parent = NULL,
    .data = NULL,
    .flags = RUBY_TYPED_FREE_IMMEDIATELY,
};

static inline void *rdxi_graph_from_method_reference(VALUE self, MethodReferenceData **out_data) {
    MethodReferenceData *data;
    TypedData_Get_Struct(self, MethodReferenceData, &method_reference_type, data);

    void *graph;
    TypedData_Get_Struct(data->graph_obj, void *, &graph_type, graph);

    *out_data = data;
    return graph;
}

static VALUE rdxr_method_reference_alloc(VALUE klass) {
    MethodReferenceData *data = ALLOC(MethodReferenceData);
    *data = (MethodReferenceData) {
        .graph_obj = Qnil,
        .cached_name = Qundef,
        .id = 0,
        .cached_epoch = 0,
    };
    return TypedData_Wrap_Struct(klass, &method_reference_type, data);
}

static VALUE rdxr_method_reference_initialize(VALUE self, VALUE graph_obj, VALUE id_val) {
    MethodReferenceData *data;
    TypedData_Get_Struct(self, MethodReferenceData, &method_reference_type, data);
    *data = (MethodReferenceData) {
        .graph_obj = graph_obj,
        .cached_name = Qundef,
        .id = NUM2ULL(id_val),
        .cached_epoch = 0,
    };
    return self;
}

VALUE rdxi_method_reference_new(VALUE graph_obj, uint64_t id, VALUE cached_name, uint64_t cached_epoch) {
    MethodReferenceData *data = ALLOC(MethodReferenceData);
    *data = (MethodReferenceData) {
        .graph_obj = graph_obj,
        .cached_name = cached_name,
        .id = id,
        .cached_epoch = cached_epoch,
    };
    return TypedData_Wrap_Struct(cMethodReference, &method_reference_type, data);
}

/*
 * call-seq:
 *   name -> String
 *
 * Returns the unresolved constant name.
 */
static VALUE rdxr_constant_reference_name(VALUE self) {
    HandleData *data;
    void *graph = rdxi_graph_from_handle(self, &data);

    const char *name = rdx_constant_reference_name(graph, data->id);
    if (name == NULL) {
        rb_raise(rb_eRuntimeError, "Constant reference must exist for a valid id");
    }
    return rdxi_owned_c_string_to_ruby(name);
}

/*
 * call-seq:
 *   location -> Rubydex::Location
 *
 * Returns the source location for this constant reference.
 */
static VALUE rdxr_constant_reference_location(VALUE self) {
    HandleData *data;
    void *graph = rdxi_graph_from_handle(self, &data);

    Location *loc = rdx_constant_reference_location(graph, data->id);
    if (loc == NULL) {
        rb_raise(rb_eRuntimeError, "Constant reference must exist for a valid id");
    }
    VALUE location = rdxi_build_location_value(loc);
    rdx_location_free(loc);
    return location;
}

/*
 * call-seq:
 *   document -> Rubydex::Document
 *
 * Returns the document this constant reference belongs to.
 */
static VALUE rdxr_constant_reference_document(VALUE self) {
    HandleData *data;
    void *graph = rdxi_graph_from_handle(self, &data);

    const uint64_t *uri_id = rdx_constant_reference_document(graph, data->id);
    if (uri_id == NULL) {
        rb_raise(rb_eRuntimeError, "Constant reference not found");
    }

    VALUE argv[] = {data->graph_obj, ULL2NUM(*uri_id)};
    free_u64(uri_id);

    return rb_class_new_instance(2, argv, cDocument);
}

/*
 * call-seq:
 *   name -> String
 *
 * Returns the referenced method name.
 */
static VALUE rdxr_method_reference_name(VALUE self) {
    MethodReferenceData *data;
    void *graph = rdxi_graph_from_method_reference(self, &data);

    if (data->cached_name != Qundef && data->cached_epoch == rdx_graph_mutation_epoch(graph)) {
        return rb_str_dup(data->cached_name);
    }

    OwnedString name = rdx_method_reference_name(graph, data->id);
    if (name.data == NULL) {
        rb_raise(rb_eRuntimeError, "Method reference must exist for a valid id");
    }
    return rdxi_owned_string_to_ruby(name);
}

/*
 * call-seq:
 *   location -> Rubydex::Location
 *
 * Returns the source location for this method reference.
 */
static VALUE rdxr_method_reference_location(VALUE self) {
    MethodReferenceData *data;
    void *graph = rdxi_graph_from_method_reference(self, &data);

    Location *loc = rdx_method_reference_location(graph, data->id);
    if (loc == NULL) {
        rb_raise(rb_eRuntimeError, "Method reference must exist for a valid id");
    }
    VALUE location = rdxi_build_location_value(loc);
    rdx_location_free(loc);
    return location;
}

/*
 * call-seq:
 *   receiver -> Rubydex::Declaration?
 *
 * Returns the resolved declaration for the receiver of the method call. Returns nil when the receiver is not a tracked
 * constant or cannot be resolved.
 */
static VALUE rdxr_method_reference_receiver(VALUE self) {
    MethodReferenceData *data;
    void *graph = rdxi_graph_from_method_reference(self, &data);

    const struct CDeclaration *decl = rdx_method_reference_receiver_declaration(graph, data->id);
    if (decl == NULL) {
        return Qnil;
    }

    VALUE decl_class = rdxi_declaration_class_for_kind(decl->kind);
    VALUE argv[] = {data->graph_obj, ULL2NUM(decl->id)};
    free_c_declaration(decl);

    return rb_class_new_instance(2, argv, decl_class);
}

/*
 * call-seq:
 *   document -> Rubydex::Document
 *
 * Returns the document this method reference belongs to.
 */
static VALUE rdxr_method_reference_document(VALUE self) {
    MethodReferenceData *data;
    void *graph = rdxi_graph_from_method_reference(self, &data);

    const uint64_t *uri_id = rdx_method_reference_document(graph, data->id);
    if (uri_id == NULL) {
        rb_raise(rb_eRuntimeError, "Method reference not found");
    }

    VALUE argv[] = {data->graph_obj, ULL2NUM(*uri_id)};
    free_u64(uri_id);

    return rb_class_new_instance(2, argv, cDocument);
}

/*
 * call-seq:
 *   declaration -> Rubydex::Declaration
 *
 * Returns the resolved declaration.
 */
static VALUE rdxr_resolved_constant_reference_declaration(VALUE self) {
    HandleData *data;
    void *graph = rdxi_graph_from_handle(self, &data);

    const struct CDeclaration *decl = rdx_resolved_constant_reference_declaration(graph, data->id);
    if (decl == NULL) {
        rb_raise(rb_eRuntimeError, "Invalid declaration for a resolved constant reference");
    }

    VALUE decl_class = rdxi_declaration_class_for_kind(decl->kind);
    VALUE argv[] = {data->graph_obj, ULL2NUM(decl->id)};
    free_c_declaration(decl);

    return rb_class_new_instance(2, argv, decl_class);
}

void rdxi_initialize_reference(VALUE mRubydex) {
    cDocument = rb_const_get(mRubydex, rb_intern("Document"));

    cReference = rb_define_class_under(mRubydex, "Reference", rb_cObject);
    rb_define_alloc_func(cReference, rdxr_handle_alloc);
    rb_define_method(cReference, "initialize", rdxr_handle_initialize, 2);
    rb_funcall(rb_singleton_class(cReference), rb_intern("private"), 1, ID2SYM(rb_intern("new")));

    cConstantReference = rb_define_class_under(mRubydex, "ConstantReference", cReference);
    rb_define_alloc_func(cConstantReference, rdxr_handle_alloc);
    rb_define_method(cConstantReference, "initialize", rdxr_handle_initialize, 2);
    rb_define_method(cConstantReference, "location", rdxr_constant_reference_location, 0);
    rb_define_method(cConstantReference, "document", rdxr_constant_reference_document, 0);
    rb_funcall(rb_singleton_class(cConstantReference), rb_intern("private"), 1, ID2SYM(rb_intern("new")));

    cUnresolvedConstantReference = rb_define_class_under(mRubydex, "UnresolvedConstantReference", cConstantReference);
    rb_define_alloc_func(cUnresolvedConstantReference, rdxr_handle_alloc);
    rb_define_method(cUnresolvedConstantReference, "name", rdxr_constant_reference_name, 0);

    cResolvedConstantReference = rb_define_class_under(mRubydex, "ResolvedConstantReference", cConstantReference);
    rb_define_alloc_func(cResolvedConstantReference, rdxr_handle_alloc);
    rb_define_method(cResolvedConstantReference, "declaration", rdxr_resolved_constant_reference_declaration, 0);

    cMethodReference = rb_define_class_under(mRubydex, "MethodReference", cReference);
    rb_define_alloc_func(cMethodReference, rdxr_method_reference_alloc);
    rb_define_method(cMethodReference, "initialize", rdxr_method_reference_initialize, 2);
    rb_define_method(cMethodReference, "name", rdxr_method_reference_name, 0);
    rb_define_method(cMethodReference, "location", rdxr_method_reference_location, 0);
    rb_define_method(cMethodReference, "receiver", rdxr_method_reference_receiver, 0);
    rb_define_method(cMethodReference, "document", rdxr_method_reference_document, 0);
}
