import {
  useIsMutating,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { codexAutoSwitchApi } from "@/lib/api/codexAutoSwitch";

export const codexAutoSwitchKeys = {
  status: ["codexAutoSwitch", "status"] as const,
  failureHistory: ["codexAutoSwitch", "failureHistory"] as const,
  control: ["codexAutoSwitch", "control"] as const,
};

export function useCodexAutoSwitch() {
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: codexAutoSwitchKeys.status,
    queryFn: codexAutoSwitchApi.getStatus,
    // Display refresh only. The native service monitors independently of this query.
    refetchInterval: 2000,
    refetchOnWindowFocus: true,
    retry: false,
  });
  const failureHistory = useQuery({
    queryKey: codexAutoSwitchKeys.failureHistory,
    queryFn: codexAutoSwitchApi.getFailureHistory,
    // The native service owns recording; this only keeps the visible audit
    // list fresh when the panel is open.
    refetchInterval: 5000,
    refetchOnWindowFocus: true,
    retry: false,
  });
  const control = useMutation({
    mutationKey: codexAutoSwitchKeys.control,
    mutationFn: (action: { enabled: boolean } | { cancel: true }) =>
      "cancel" in action
        ? codexAutoSwitchApi.cancel()
        : codexAutoSwitchApi.setEnabled(action.enabled),
    onSettled: async () => {
      // Reconcile uncertain replies too; never infer success from a local toggle.
      await queryClient.invalidateQueries({
        queryKey: codexAutoSwitchKeys.status,
      });
    },
  });
  const pendingControls = useIsMutating({
    mutationKey: codexAutoSwitchKeys.control,
  });

  return {
    status,
    failureHistory,
    control,
    isPending: pendingControls > 0,
  };
}
