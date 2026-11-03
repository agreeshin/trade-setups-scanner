import { configureStore } from "@reduxjs/toolkit";
import { appSlice } from "./app-slice";
export const createStore = () =>
  configureStore({
    reducer: {
      app: appSlice.reducer,
    },
  });
export const store = createStore();
export type AppStore = ReturnType<typeof createStore>;
export type RootState = ReturnType<AppStore["getState"]>;
export type AppDispatch = AppStore["dispatch"];
