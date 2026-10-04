#include <cassert>
#include <iostream>

#include "hello/greet.h"

int main() {
    assert(greet("test") == "Hello, test!");
    assert(greet("") == "Hello, !");
    std::cout << "all cxx tests passed" << std::endl;
    return 0;
}
