import { createSlice, type PayloadAction } from "@reduxjs/toolkit";
export type ServiceStatus = "unknown" | "ready" | "degraded";
export interface AppState {
  marketDataStatus: ServiceStatus;
}

const initialState: AppState = {
  marketDataStatus: "unknown",
};

export const appSlice = createSlice({
  name: "app",
  initialState,
  reducers: {
    marketDataStatusChanged(state, action: PayloadAction<ServiceStatus>) {
      state.marketDataStatus = action.payload;
    },
  },
});
export const { marketDataStatusChanged } = appSlice.actions;
