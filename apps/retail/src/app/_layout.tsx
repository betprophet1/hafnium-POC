// The app, on all three platforms. One stack of screens, one venue session under them, and a layout that is the same
// code on a phone and in a browser: what changes with the width is the width, not the app.

import { DarkTheme, DefaultTheme, Stack, ThemeProvider } from 'expo-router';
import { useColorScheme } from 'react-native';

import { VenueProvider } from '@/venue/session';

export default function RootLayout() {
  const scheme = useColorScheme();
  return (
    <VenueProvider>
      <ThemeProvider value={scheme === 'dark' ? DarkTheme : DefaultTheme}>
        <Stack screenOptions={{ headerLargeTitle: false }}>
          <Stack.Screen name="index" options={{ title: 'Markets' }} />
          <Stack.Screen name="market/[id]" options={{ title: 'Market' }} />
          <Stack.Screen name="account" options={{ title: 'Account' }} />
        </Stack>
      </ThemeProvider>
    </VenueProvider>
  );
}
