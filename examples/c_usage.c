#include <stdio.h>
#include <stdlib.h>

#include "cJSON.h"

int main(void) {
    const char *input = "{\"project\":\"cjson-rs\",\"version\":1,\"features\":[\"safe-rust\",\"c-abi\"]}";
    cJSON *parsed = cJSON_Parse(input);
    if (parsed == NULL) {
        const char *err = cJSON_GetErrorPtr();
        fprintf(stderr, "Parse error near: %s\n", err != NULL ? err : "(null)");
        return 1;
    }

    const cJSON *name = cJSON_GetObjectItemCaseSensitive(parsed, "project");
    if (cJSON_IsString(name) && name->valuestring != NULL) {
        printf("Parsed project: %s\n", name->valuestring);
    }

    cJSON *root = cJSON_CreateObject();
    if (root == NULL) {
        cJSON_Delete(parsed);
        return 1;
    }

    cJSON_AddStringToObject(root, "status", "ok");
    cJSON_AddNumberToObject(root, "abi_patch", 19);
    cJSON_AddTrueToObject(root, "memory_safe");

    cJSON *items = cJSON_AddArrayToObject(root, "items");
    if (items != NULL) {
        cJSON_AddItemToArray(items, cJSON_CreateString("parse"));
        cJSON_AddItemToArray(items, cJSON_CreateString("print"));
    }

    char *formatted = cJSON_Print(root);
    char *compact = cJSON_PrintUnformatted(root);
    if (formatted != NULL && compact != NULL) {
        printf("Formatted:\n%s\n", formatted);
        printf("Compact:\n%s\n", compact);
    }

    cJSON_free(formatted);
    cJSON_free(compact);
    cJSON_Delete(root);
    cJSON_Delete(parsed);
    return 0;
}
