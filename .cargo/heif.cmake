if(NOT "$ENV{DNG_MONO_MUSL_ROOT}" STREQUAL "")
    set(CMAKE_SYSTEM_NAME Linux)
    set(CMAKE_SYSTEM_PROCESSOR "$ENV{DNG_MONO_MUSL_PROCESSOR}")
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
