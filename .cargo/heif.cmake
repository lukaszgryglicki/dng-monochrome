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
