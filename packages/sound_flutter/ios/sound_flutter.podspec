#
# To learn more about a Podspec see http://guides.cocoapods.org/syntax/podspec.html.
# Run `pod lib lint sound_flutter.podspec` to validate before publishing.
#
Pod::Spec.new do |s|
  s.name             = 'sound_flutter'
  s.version          = '0.0.1'
  s.summary          = 'Cross-platform audio playback for Flutter.'
  s.description      = <<-DESC
Builds the sound_cli Rust audio library via cargokit and links it into the
sound_flutter plugin for use over FFI.
                       DESC
  s.homepage         = 'https://github.com/ManyMath/sound'
  s.license          = { :file => '../LICENSE' }
  s.author           = 'ManyMath LLC'

  s.source           = { :path => '.' }
  s.source_files     = 'Classes/**/*'

  # Build the shared `sound_cli` Rust crate (static lib) via cargokit and
  # force-link it into the plugin framework.
  s.script_phase = {
    :name => 'Build sound_cli',
    :script => 'sh "$PODS_TARGET_SRCROOT/../cargokit/build_pod.sh" ../../../native/sound_cli sound_cli',
    :execution_position => :before_compile,
    :input_files => ['${BUILT_PRODUCTS_DIR}/cargokit_phony'],
    :output_files => ['${BUILT_PRODUCTS_DIR}/libsound_cli.a'],
  }

  s.dependency 'Flutter'
  s.platform = :ios, '13.0'

  # Flutter.framework does not contain a i386 slice.
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'EXCLUDED_ARCHS[sdk=iphonesimulator*]' => 'i386',
    'OTHER_LDFLAGS' => '-force_load ${BUILT_PRODUCTS_DIR}/libsound_cli.a',
  }
  s.swift_version = '5.0'
end
