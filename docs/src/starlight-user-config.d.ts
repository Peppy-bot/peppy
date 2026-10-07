// Starlight serves its validated configuration as the `virtual:starlight/user-config`
// module but publishes no type declaration for it. SocialIcons.astro reads the social
// links from that module, so this file types it with Starlight's public
// `StarlightConfig` type.
declare module 'virtual:starlight/user-config' {
  const config: import('@astrojs/starlight/types').StarlightConfig;
  export default config;
}
