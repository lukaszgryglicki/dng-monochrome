if(NOT "$ENV{DNG_MONO_MUSL_ROOT}" STREQUAL "")
    set(CMAKE_SYSTEM_NAME Linux)
    set(CMAKE_SYSTEM_PROCESSOR "$ENV{DNG_MONO_MUSL_PROCESSOR}")
    set(musl_compiler_prefix
        "$ENV{DNG_MONO_MUSL_TOOLCHAIN}/bin/$ENV{DNG_MONO_MUSL_PROCESSOR}-linux-musl-")
    set(CMAKE_C_COMPILER "${musl_compiler_prefix}gcc" CACHE FILEPATH "" FORCE)
    set(CMAKE_CXX_COMPILER "${musl_compiler_prefix}g++" CACHE FILEPATH "" FORCE)
    set(CMAKE_AR "${musl_compiler_prefix}gcc-ar" CACHE FILEPATH "" FORCE)
    if("$ENV{CARGO_PKG_NAME}" STREQUAL "libheif-sys")
        # CMake drops caller options when replacing a cached compiler.
        foreach(setting
            BUILD_SHARED_LIBS BUILD_TESTING BUILD_DOCUMENTATION WITH_GDK_PIXBUF
            WITH_EXAMPLES WITH_EXAMPLE_HEIF_THUMB WITH_EXAMPLE_HEIF_VIEW
            ENABLE_EXPERIMENTAL_FEATURES ENABLE_PLUGIN_LOADING
        )
            set(${setting} OFF CACHE BOOL "" FORCE)
        endforeach()
        if("$ENV{OUT_DIR}" STREQUAL "")
            message(FATAL_ERROR "Missing Cargo OUT_DIR for bundled libheif")
        endif()
        set(CMAKE_BUILD_TYPE Release CACHE STRING "" FORCE)
        set(CMAKE_INSTALL_LIBDIR lib CACHE STRING "" FORCE)
        set(CMAKE_INSTALL_PREFIX "$ENV{OUT_DIR}/libheif_build" CACHE PATH "" FORCE)
    endif()
    set(CMAKE_FIND_ROOT_PATH
        "$ENV{DNG_MONO_MUSL_ROOT}/x265/install"
        "$ENV{DNG_MONO_MUSL_ROOT}/aom"
        "$ENV{DNG_MONO_MUSL_TOOLCHAIN}")
    set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
    set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
    set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
    set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
endif()

# Keep HEVC/AV1 only; unused OpenH264 also has broken FreeBSD C++ linker metadata.
foreach(codec
    X264 OpenH264_DECODER RAV1E SvtEnc KVAZAAR
    JPEG_DECODER JPEG_ENCODER OpenJPEG_DECODER OpenJPEG_ENCODER
    OPENJPH_ENCODER OPEN_JPH_ENCODER FFMPEG_DECODER UVG266 VVDEC VVENC
)
    set(WITH_${codec} OFF CACHE BOOL "" FORCE)
endforeach()

if("$ENV{DNG_MONO_STATIC}" STREQUAL "1")
    # The converter only encodes HEIF; avoid requiring static decoder libraries.
    foreach(codec LIBDE265 DAV1D AOM_DECODER)
        set(WITH_${codec} OFF CACHE BOOL "" FORCE)
    endforeach()
endif()
